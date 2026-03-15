# Product Specification & Production-Readiness Plan
## Polymarket Arbitrage Trading System

**Author**: Chief Product Manager
**Audience**: Head Programmer (AI-assisted implementation via Claude)
**Date**: 2026-03-14
**Status**: APPROVED by CTO 2026-03-14

---

## 1. Context & Problem Statement

A senior developer built an automated prediction market arbitrage system under pressure and without a formal spec. The developer is now off sick. The CTO needs:

1. **A product spec** derived from reverse-engineering the codebase and the 1,218-line PROGRESS_ROADMAP.md
2. **A logically sequenced plan** to take the system from its current SHADOW mode to production (live trading with real money)

### CTO Decisions (confirmed)
- **Product scope**: Personal tool now; future commercial potential (model undecided). Note extensibility points, don't over-engineer.
- **Implementer**: AI-assisted (Claude). Spec structured as task breakdowns with acceptance criteria.
- **Risk appetite**: 10% drawdown circuit breaker. $1,000 initial shadow capital. Go live after 2 weeks of profitable shadow trading with $1,000.
- **Timeline**: No rush. Get it right. Shadow mode runs indefinitely until solid.
- **Production target**: VPS only (ZAP-Hosting, Germany, always-on, 4 vCPU, **8 GB RAM**, 25 GB NVMe). Laptop is dev-only.
- **Rust port**: Complete before go-live. Single compiled binary is the target state before real money flows.
- **Notifications**: WhatsApp (not OpenClaw, not email).
- **Shadow strategy**: Run 5 parallel shadow accounts with different parameter combinations to optimise % capital per position and max concurrent positions before committing real money.

---

## 2. Product Definition

### 2.1 What It Does
Automated detection and execution of guaranteed-profit arbitrage on Polymarket prediction markets. Identifies groups of mutually exclusive outcomes whose prices sum to != $1.00 and places bets across all outcomes to lock in a risk-free profit regardless of result.

**This is not prediction or gambling. It is pricing-error exploitation with mathematically guaranteed returns.**

### 2.2 How It Works (simplified)
1. **Scan**: Fetch all 33k+ active markets from Polymarket
2. **Detect**: Find mutex groups (outcomes where exactly one must win)
3. **Evaluate**: Continuously monitor prices via WebSocket; when a group's prices drift from fair value, compute optimal bet sizes
4. **Execute**: Place orders across all legs simultaneously to lock in the arb
5. **Manage**: Track positions until markets resolve; replace with better opportunities when found
6. **Resolve**: When outcome is determined, collect payout. Guaranteed profit if execution was correct.

### 2.3 Key Metrics
| Metric | Current (Shadow) | Target (Live) |
|--------|-----------------|---------------|
| Initial capital | $100 | $1,000 |
| Net gain | +$8.83 (8.8%) | Track after go-live |
| Win rate (resolved arbs) | 100% (3/3) | Maintain 100% |
| Open positions | 10 | Optimised via shadow testing (see Milestone D) |
| Eval latency (p50) | 1–5 ms | < 10 ms |
| Market coverage | 52% (18.5k/35.8k assets) | Maintain or improve |
| Uptime | Manual restarts | 99%+ (systemd, auto-restart) |

### 2.4 Architecture (current state: v0.04.24)

**Hybrid Python/Rust system:**
- **Rust (hot path)**: WebSocket connections, order book mirror, EFP computation, eval queue, arb math, position manager, state persistence (SQLite), dashboard (axum HTTP + SSE)
- **Python (orchestration)**: Main supervisor, market scanner, constraint detection, AI resolution validator, AI postponement detector, live trading executor, config loading

**Target state (pre go-live)**: Single compiled Rust binary. Zero Python runtime dependency.

**Deployment**: VPS — 4 vCPU, 8 GB RAM, 25 GB NVMe, Ubuntu 24.04, systemd

### 2.5 Trading Modes
| Mode | Behaviour |
|------|-----------|
| **SHADOW** | Paper trades validated against live order book. No money at risk. Current mode. |
| **LIVE** | Real money via Polymarket CLOB API. Target mode after validation. |

> Note: PAPER mode was retired in v0.04.07. Shadow is the minimum operating mode.

---

## 3. Document Audit: Issues Found in PROGRESS_ROADMAP.md

The following inconsistencies were identified. These will be addressed by the documentation deliverables embedded within Milestones A–C (see Documentation Schedule in Section 5).

| # | Issue | Detail |
|---|-------|--------|
| 1 | **Stale header** | Version says v0.04.14; actual is v0.04.24. "Last updated" is also stale. |
| 2 | **Section numbering collision** | Roadmap references "§7.1" for Architecture Evolution, but document §7 is Version History. |
| 3 | **Phase numbering doesn't reflect execution order** | Phase 8 is 90% done; Phase 2 is 0%. Phases were never executed in order. |
| 4 | **Contradictory statuses** | Phase 7.3 (Rust Port) marked "deferred" but Phase 8 P4 (which IS the Rust port) is largely complete. |
| 5 | **Rust PositionManager not reflected** | v0.04.16–18 built Rust PM, but Phase 2.1 still shows refactoring as "Not Started". |
| 6 | **Dashboard description outdated** | §2.1 lists `dashboard_server.py`; reality is Rust axum dashboard since v0.04.20. |
| 7 | **PAPER mode still documented** | Retired in v0.04.07 but §2.5 still lists it. |
| 8 | **Redundant performance data** | Same latency metrics in §2.1, Phase 8 roadmap, and §9 with overlapping but non-identical figures. |
| 9 | **Legacy files in structure** | File tree lists `state_db.py`, `websocket_manager.py`, `dashboard_server.py` which may be superseded by Rust equivalents. |
| 10 | **Tag list out of order** | §12 lists v0.04.13 after v0.04.14. |
| 11 | **Document serves too many purposes** | User guide + architecture + roadmap + incident log + version history + ops runbook in one 1,218-line file. |
| 12 | **Hardcoded magic numbers** | Thresholds like min_price_sum (0.85), mutex guard (0.90), EFP drift ($0.005), book staleness (5s/30s), and others are hardcoded in source rather than driven by config. |

---

## 4. Capability Audit

### 4.1 Working and Proven
- Market scanning (33k+ markets, full pagination)
- Constraint detection (mutex group finder with completeness guards)
- Arb math (Rust: direct mutex + polytope Frank-Wolfe, 4.2 us/eval)
- WebSocket (Rust tokio-tungstenite, 9 shards, ~2,148 msg/s)
- Order book mirror (Rust DashMap, EFP drift detection)
- Eval pipeline (full Rust hot path, p50 = 1–5 ms)
- Position lifecycle (Rust: entry, replacement, resolution)
- State persistence (rusqlite in-memory + WAL, survives restarts)
- Dashboard (Rust axum + SSE, zero disk reads, live updates)
- AI resolution validator (Anthropic API, 1-week cache)
- AI postponement detector (web search for rescheduled events)
- Shadow mode validation (paper trades cross-checked against live books)

### 4.2 Designed but Not Implemented
- Pre-trade validation (re-read books at execution time)
- negRisk sell arb capital calculations
- Order book depth gating for entry/replacement
- Partial fill handling (score-based unwind)
- FAK live orders
- Replacement chain tracking analytics
- Notification alerts
- Dashboard control panel (mode switch via UI)

### 4.3 Not Yet Designed
- Live P&L tracking on dashboard
- Position reconciliation (CLOB fills vs internal record)
- Circuit breaker / auto-halt
- Monitoring and alerting beyond dashboard
- Disaster recovery / rollback
- Multi-shadow parameter optimisation (5 parallel accounts — see Milestone D)

---

## 5. Production-Readiness Plan (Sequenced Milestones)

### Overview

```
🔧 Milestone A: Complete Rust Port + parameterise config + docs (CHANGELOG, INCIDENT_LOG)
    |
⬚ Milestone B: Live Execution Path + docs (ARCHITECTURE, ROADMAP)
    |
⬚ Milestone C: Safety Infrastructure + docs (OPS_RUNBOOK, USER_GUIDE) + retire PROGRESS_ROADMAP
    |
⬚ Milestone D: Shadow Validation (2 weeks, 5x shadow accounts, parameter optimisation)
    |
⬚ Milestone E: Go Live ($1,000, supervised, VPS)
    |
⬚ Milestone F: Stabilise & Scale (depth gating, partial fills, more capital)
```

Documentation deliverables are woven into Milestones A–C at the point where their content becomes stable.

### Documentation Schedule

Each document is produced at the point in the milestone sequence when its content is settled and accurate.

| Document | Produced at | Rationale |
|----------|------------|-----------|
| **CHANGELOG.md** | End of Milestone A | Version history is stable; the Rust port is the last major architectural change before go-live. Write it while the full history is fresh. |
| **INCIDENT_LOG.md** | End of Milestone A | All 6 incidents are historical and fully resolved. No dependency on later milestones. |
| **ARCHITECTURE.md** | End of Milestone B | By this point the Rust port is complete (A) and the live execution path is built (B). The architecture is in its final state. Writing it earlier would require rewriting after B. |
| **ROADMAP.md** | End of Milestone B | With A and B complete, the roadmap can accurately reflect what's done vs remaining (C, D, E, F). |
| **OPS_RUNBOOK.md** | End of Milestone C | Safety infrastructure (circuit breaker, kill switch, WhatsApp, scripts) must exist before the runbook can document how to operate them. |
| **USER_GUIDE.md** | End of Milestone C | Depends on: rationalised scripts (A9), live execution (B), safety features (C). This is the last document because it must describe the complete operational system. |
| **Retire PROGRESS_ROADMAP.md** | End of Milestone C | All 6 replacement documents exist. Replace contents with a pointer to the new docs. |

---

### 🔧 Milestone A: Complete Rust Port

**Goal**: Single compiled Rust binary. Zero Python runtime. Simpler deployment, fewer failure modes, better performance. Parameterise all hardcoded thresholds.

**Why before go-live**: The CTO decided real money should only flow through the cleanest possible system. The hybrid Python/Rust boundary is a source of complexity and potential bugs.

**Critical files**:
- `rust_engine/src/lib.rs` — PyO3 bindings (to be replaced with native Rust main)
- `rust_engine/src/` — all Rust modules
- `trading_engine.py` — Python orchestrator (to be replaced)
- `paper_trading.py` — Python position lifecycle (already partially replaced by Rust PM)
- `resolution_validator.py` — AI validation (to be ported)
- `postponement_detector.py` — AI detection (to be ported)
- `layer1_runner.py` — market scanner (to be ported)
- `layer2_constraint_detection/constraint_detector.py` — constraint detection (to be ported)
- `config/config.yaml` + `config/secrets.yaml` — config loading (to be ported)
- `main.py` — supervisor (to be replaced by Rust main)

| Task | Acceptance Criteria |
|------|-------------------|
| ✅ **A1: Remove paper_engine middleman** (8q-7) | Trading engine calls Rust PositionManager directly, no Python `paper_trading.py` in the hot path. All position operations (enter, replace, resolve, liquidate) go through Rust. |
| ✅ **A2: Port resolution validator** (8q-8) | Rust module makes Anthropic API calls with same prompt templates from `config/prompts.yaml`. Cache results in SQLite (same TTL). Returns same structured response. Existing test cases pass. |
| ✅ **A3: Port postponement detector** (8q-9) | Rust module performs web search via Anthropic API with two-attempt strategy. Same config-driven prompts. Results stored in position metadata. Rate limiting respected (60s between calls). |
| ✅ **A4: Port market scanner** (8q-10) | Rust module fetches all markets from Gamma API with pagination. Writes to SQLite (not JSON). Runs once at startup + periodic refresh. Handles API errors gracefully. |
| ✅ **A5: Port constraint detector** (8q-11) | Rust module identifies mutex groups from market data. Completeness guards read from config (not hardcoded). Results stored in Rust ConstraintStore. |
| ✅ **A6: Port config loading + parameterise all thresholds** (8q-13) | Rust reads `config.yaml` and `secrets.yaml` at startup. **All previously hardcoded thresholds moved to config** (see Parameterisation Table below). Hot-reload not required (restart is acceptable). |
| ✅ **A7: Port logging** (8q-14) | Rust `tracing` crate with `tracing-appender`. Daily rotating files (`rust_engine.YYYY-MM-DD`). Level configurable via `config.yaml`. Auto-cleanup by retention days. All `eprintln!` replaced with tracing macros. |
| ✅ **A8: Port main orchestrator** (8q-12) | `rust_supervisor` crate: 2.1MB binary replaces `main.py`. PID lock, SIGTERM/SIGINT handling via `nix`, subprocess monitoring with auto-restart, log cleanup. Reads config from `config.yaml`. Systemd compatible. |
| ✅ **A9: Rationalise scripts** | 3 essential scripts: `start.sh` (build if needed + P: mount + start supervisor + verify), `restart.sh` (kill + pull + rebuild + start), `kill.sh` (SIGTERM + SIGKILL + optional --cancel CLOB orders). Windows .bat files updated to delegate to .sh. VPS `setup_vps.sh` updated for Rust binary + systemd. Supervisor supports `--mode`, `--set key=value`, `--dry-run` CLI args. Legacy scripts archived. |
| ⬚ **A10: Single compiled binary** (8q-15) | `cargo build --release` produces one binary. Deploy to VPS with config files only. No Python, no venv, no pip. |
| ⬚ **A11: Create CHANGELOG.md** | All version entries (v0.01.00 through current) transferred from PROGRESS_ROADMAP.md §7. Compare with git records and update. Most recent first. keepachangelog.com format (Added/Changed/Fixed/Removed). |
| ⬚ **A12: Create INCIDENT_LOG.md** | All incidents (INC-001 through INC-006+) transferred. Each has: date, markets, impact, root cause, fix, status. Includes template for future incidents. |

#### Parameterisation Table

All of the following values are currently hardcoded in source files. Task A6 moves them to `config.yaml` with sensible defaults matching current behaviour.

Convention: **0 means "no filter / disabled"** for any threshold parameter. This lets operators remove a gate without deleting the key.

| Parameter | Current location | Hardcoded value | Proposed config path |
|-----------|-----------------|-----------------|---------------------|
| **Detection & Scoring** | | | |
| L2 min price sum | `constraint_detector.py` | 0.85 | `constraints.min_price_sum` |
| L2 max price sum | `constraint_detector.py` | 1.15 | `constraints.max_price_sum` |
| L2 min markets per group | `constraint_detector.py` | 2 | `constraints.min_markets` |
| L3 direct mutex guard | `arbitrage_engine.py` | 0.90 | `constraints.mutex_completeness_guard` |
| L3 polytope mutex guard | `arbitrage_engine.py` | 0.90 | `constraints.polytope_completeness_guard` |
| L3 min individual price | `arbitrage_engine.py` | $0.02 | `constraints.min_individual_price` |
| L3 max polytope markets | `arbitrage_engine.py` | 12 | `constraints.max_polytope_markets` |
| Constraint rebuild interval | `trading_engine.py` | 600 s | `engine.constraint_rebuild_interval_seconds` |
| **Entry & Ranking Filters** | | | |
| Min resolution time (shadow) | `trading_engine.py` | 300 s | `arbitrage.min_resolution_time_secs` |
| Min resolution time (live pre-trade) | `config.yaml` (already) | 3600 s | `live_trading.min_resolution_time_secs` (keep) |
| Max days to resolution (entry) | `config.yaml` (already) | 60 d | `arbitrage.max_days_to_resolution` (keep) |
| Max days to replacement | `config.yaml` (already) | 30 d | `arbitrage.max_days_to_replacement` (keep) |
| Min trade size | `trading_engine.py` | $10 | `arbitrage.min_trade_size` |
| Max evals per batch | `trading_engine.py` | 500 | `engine.max_evals_per_batch` |
| **EFP & Book** | | | |
| EFP drift threshold (urgent queue) | `trading_engine.py` | $0.005 | `engine.efp_drift_threshold` |
| EFP background staleness | `trading_engine.py` | 5 s | `engine.efp_staleness_seconds` |
| Book staleness threshold (pre-trade) | designed (Phase 7.2) | 5 s | `engine.book_staleness_seconds` |
| **WebSocket** | | | |
| WS stale-asset re-subscribe interval | `trading_engine.py` | 60 s | `websocket.stale_resub_interval_seconds` |
| WS stale-asset threshold | `trading_engine.py` | 30 s | `websocket.stale_threshold_seconds` |
| WS assets per shard | `trading_engine.py` | 2,000 | `websocket.assets_per_shard` |
| **Position Lifecycle** | | | |
| Replacement cooldown | `trading_engine.py` | 60 s | `arbitrage.replacement_cooldown_seconds` |
| Replacement protection window | `paper_trading.py` | 24 h | `arbitrage.replacement_protection_hours` |
| Postponement check interval | `trading_engine.py` | 3600 s | `ai.postponement.check_interval_seconds` |
| Postponement rate limit | `config.yaml` (already) | 60 s | `ai.postponement.rate_limit_seconds` (keep) |
| Postponement overdue threshold | `trading_engine.py` | 24 h | `ai.postponement.overdue_hours` |
| **Live Trading** | | | |
| Pre-trade profit abort ratio | designed (Phase 7.2) | 0.70 | `live_trading.min_profit_ratio` |
| Depth haircut (phantom order allowance) | `orderbook_depth.py` | 0.80 | `live_trading.depth_haircut` |
| Min depth per leg | designed (Phase 5a) | $5.00 | `live_trading.min_depth_per_leg` |
| Reconciliation interval | designed (Phase B5) | 5 min | `live_trading.reconciliation_interval_seconds` |
| **Safety** | | | |
| Circuit breaker drawdown | designed (Phase C1) | 10% | `safety.circuit_breaker.max_drawdown_pct` |
| Circuit breaker error count | designed (Phase C1) | 3 | `safety.circuit_breaker.max_consecutive_errors` |
| Circuit breaker error window | designed (Phase C1) | 5 min | `safety.circuit_breaker.error_window_seconds` |
| Circuit breaker API timeout | designed (Phase C1) | 60 s | `safety.circuit_breaker.api_timeout_seconds` |

**Verification**: System runs in shadow mode on VPS from single binary for 24h with zero errors. Same arb detection rate as hybrid system. Dashboard accessible. State persists across restarts. All thresholds readable from config (verified by changing one and confirming behaviour changes on restart).

---

### ⬚ Milestone B: Live Execution Path

**Goal**: The system can place real orders on Polymarket's CLOB and handle all outcomes.

**Critical files** (all Rust after Milestone A):
- Position manager (`rust_engine/src/position.rs`)
- Arb math (`rust_engine/src/arb.rs`)
- New: live executor module
- New: reconciliation module

| Task | Acceptance Criteria |
|------|-------------------|
| ⬚ **B1.0: Order book depth gating** | Integrate depth check (5a.2) into entry/replacement flow. Gate all entries on sufficient book depth per leg. Log depth-limited trades. Prevents unfillable orders before going live. |
| ⬚ **B1.1: Replacement chain analytics** | Track full chain history (5d): `chain_id`, `chain_start_time`, `chain_cumulative_fees` in position metadata. Dashboard chain view. True return = `(payout - total_fees) / chain_duration`. Informs replacement tuning. |
| ⬚ **B1.2: Record retention & pruning** | Closed positions retained indefinitely in SQLite for audit trail. AI validation reasoning stored per-position and persisted until position closes + configurable retention period (`record_retention_days`, default 90). Transient caches (resolution, postponement) pruned after TTL. On production launch: strip bulky fields (full market descriptions, raw API responses) from records older than retention period, keeping: position_id, timestamps, strategy, capital, profit, close_reason, AI reasoning summary. |
| ⬚ **B1.3: Pre-trade validation** | Before placing any live order: re-read book mirror for all legs at trade size. If any leg's book is staler than `engine.book_staleness_seconds`, fetch via REST. Abort if `actual_profit / expected_profit < live_trading.min_profit_ratio` or depth < `live_trading.min_depth_per_leg` on any leg. Log all aborts with reason. |
| ⬚ **B2: Wire LiveExecutor** | CLOB `create_order()` calls for all legs of an arb. Passes `neg_risk: true` for negRisk markets. Uses FAK (Fill And Kill) order type. Handles: success, partial fill, rejection, timeout. |
| ⬚ **B3: negRisk capital calculation** | Sell arb collateral = $1.00 per unit (not sum of NO asks). Position sizing uses correct collateral. Verified by unit test comparing with manual calculation. |
| ⬚ **B4: Partial fill handling** | After submitting all legs: check actual fills. Compute arb score of filled position. If score >= threshold: accept. If score < threshold: compute minimum unwind. If no acceptable partial: full unwind. Log all partial fill events. |
| ⬚ **B5: Position reconciliation** | On startup and every `live_trading.reconciliation_interval_seconds`: query CLOB for actual positions. Compare with internal record. Alert on any discrepancy. Log reconciliation results. |
| ⬚ **B6: Error handling matrix** | Defined behaviour for: CLOB API timeout (retry once, then abort + unwind), CLOB rejection (log + skip), network failure (pause trading, alert, retry connection), insufficient balance (pause trading, alert). |
| ⬚ **B7: Live P&L tracking** | Dashboard shows: realised P&L (closed positions), unrealised P&L (open positions marked to market), fees paid, net return %. Updated via SSE. |
| ⬚ **B8: Create ARCHITECTURE.md** | Covers: post-Rust-port architecture, data flow, component descriptions, file structure (verified against actual repo), config reference (including all parameterised values from A6), glossary. Current state only — not historical. |
| ⬚ **B9: Create ROADMAP.md** | Phases renumbered by actual priority. Each phase: goal, status, items. Cross-references ARCHITECTURE.md. No version history or incident detail. Accurately reflects A+B as complete, C–F as remaining. |

**Verification**: Run in shadow mode with live execution logging for 48h. Every "would-execute" event shows: pre-trade validation result, order details, expected fills, reconciliation status.

---

### ⬚ Milestone C: Safety Infrastructure

**Goal**: The system can protect capital automatically and alert the operator.

| Task | Acceptance Criteria |
|------|-------------------|
| ⬚ **C1: Circuit breaker** | Auto-pause trading if: (a) portfolio drawdown exceeds `safety.circuit_breaker.max_drawdown_pct` from peak, (b) `max_consecutive_errors` errors in `error_window_seconds`, (c) CLOB API unreachable for `api_timeout_seconds`. Log the trigger. Resume requires manual intervention (config change + restart). |
| ⬚ **C2: Kill switch** | `kill.sh --emergency` and dashboard button that: (a) cancels all open CLOB orders, (b) sets mode to shadow, (c) sends WhatsApp notification. Idempotent (safe to invoke multiple times). |
| ⬚ **C3: WhatsApp notifications** | Alerts via WhatsApp on: new position entry, position resolution, error, circuit breaker trigger, daily summary. Configurable in `config.yaml` (WhatsApp number, enable/disable per event type). Graceful degradation if WhatsApp API is down (log locally, don't block trading). |
| ⬚ **C4: Daily P&L report** | Automated daily summary at midnight UTC: entries, exits, fees, net P&L, capital utilisation %, drawdown from peak. Sent via WhatsApp. Also persisted to SQLite for historical queries. |
| ⬚ **C4.1: Seamless position close transition** | Eliminate 5s visual gap where a closing position disappears from open before appearing in closed. Buffer removal client-side until the closed entry arrives in the next SSE push. |
| ⬚ **C4.2: Proactive near-resolution exit** | When a held market's price approaches 1.0 (e.g. ≥0.97), scan depth-of-book to see if shares can be sold at a profit before official resolution. Useful when outcome is near-certain but resolution is delayed (e.g. temperature markets waiting for end-of-day data finalization). Depth of book will rarely allow it, but worth scanning. |
| ⬚ **C5: Create OPS_RUNBOOK.md** | VPS details, SSH access, systemd commands, log locations, dashboard URLs, monitoring commands, backup procedures, the 3 rationalised scripts and their usage, circuit breaker recovery procedure, kill switch procedure, WhatsApp setup, ZAP-Hosting 3-month login reminder. |
| ⬚ **C6: Create USER_GUIDE.md** | Product overview, prerequisites, setup from scratch, starting/stopping (rationalised scripts), monitoring (dashboard), mode switching, recovery after crash, go-live checklist, glossary. Includes: all supervisor CLI flags (`--mode`, `--set key=value`, `--dry-run` for config verification, `--port`, `--log-level`), Windows Task Scheduler setup for auto-start on reboot (VBS → SILENT.bat → start.sh → Rust supervisor), copying START_TRADER_SILENT.bat to `C:\Users\<user>\ai-workspace\`, Linux systemd setup via `setup_vps.sh`. Written so a new operator can run the system without having built it. |
| ⬚ **C7: Retire PROGRESS_ROADMAP.md** | Replace contents with a brief note listing the 6 replacement documents and their purposes. Do not delete (preserves git history). |

**Verification**: Simulate circuit breaker triggers (inject fake errors, simulate drawdown). Verify kill switch cancels test orders. Verify WhatsApp messages arrive within 60s of trigger. All 6 documents reviewed for accuracy and internal consistency.

---

### ⬚ Milestone D: Shadow Validation (2-Week Gate)

**Goal**: Prove the complete live system works without risking real money, AND determine optimal trading parameters via 5 parallel shadow accounts.

#### D-Part 1: Multi-Shadow Parameter Optimisation

Run 5 parallel shadow instances on the VPS (8 GB RAM supports this) with different parameter combinations. Each instance varies strategy parameters (not just sizing) to find the optimal balance across capital concentration, entry aggression, and replacement behaviour.

**Sizing & Concentration**

| Instance | `capital_per_trade_pct` | `max_concurrent_positions` | `max_position_size` | Shadow Capital | Strategy |
|----------|------------------------|---------------------------|---------------------|----------------|----------|
| **Shadow-A** | 5% | 40 | $500 | $1,000 | Maximum diversification |
| **Shadow-B** | 10% (current) | 20 (current) | $1,000 | $1,000 | Current baseline |
| **Shadow-C** | 20% | 15 | $1,000 | $1,000 | Moderate concentration |
| **Shadow-D** | 30% | 8 | $1,500 | $1,000 | High concentration |
| **Shadow-E** | 50% | 8 | $2,000 | $1,000 | Maximum concentration |

**Entry Filters**

| Instance | `min_profit_threshold` | `max_profit_threshold` | `min_resolution_time_secs` | `max_days_to_resolution` |
|----------|----------------------|----------------------|---------------------------|-------------------------|
| **Shadow-A** | 0.02 (2%) | 0.30 | 300 | 90 |
| **Shadow-B** | 0.03 (current) | 0.30 (current) | 300 (current) | 60 (current) |
| **Shadow-C** | 0.03 | 0.25 | 600 | 45 |
| **Shadow-D** | 0.04 | 0.20 | 900 | 30 |
| **Shadow-E** | 0.05 | 0.20 | 1800 | 21 |

**Replacement & Risk**

| Instance | `replacement_cooldown_seconds` | `max_days_to_replacement` | `max_exposure_per_market` | Notes |
|----------|-------------------------------|--------------------------|--------------------------|-------|
| **Shadow-A** | 30 | 60 | $250 | Fast replacement, broad spread |
| **Shadow-B** | 60 (current) | 30 (current) | $500 (current) | Baseline |
| **Shadow-C** | 120 | 30 | $500 | Slower replacement |
| **Shadow-D** | 120 | 21 | $750 | Slower, tighter time window |
| **Shadow-E** | 300 | 14 | $1,000 | Very selective replacement |

Shadow-A tests whether wider entry gates and faster replacement capture more value. Shadow-D/E test whether stricter filters and larger positions outperform despite fewer trades. Shadow-B remains the unchanged baseline.

Each instance runs independently with its own SQLite database and log files. Dashboard shows a comparison view of all 5.

| Task | Acceptance Criteria |
|------|-------------------|
| ⬚ **D1: Multi-instance support** | Single binary accepts `--instance <name>` flag. Each instance uses separate config overlay, separate SQLite DB, separate log directory, separate dashboard port. Shared WebSocket connections (all instances read from the same market data feed to avoid 5x WS load). |
| ⬚ **D2: Deploy 5 instances to VPS** | Five systemd services: `trader-shadow-{a..e}`. Each running with its parameter set. All 5 dashboards accessible. |

#### D-Part 1.5: Engine Stress Testing

Before building the comparison dashboard, stress-test the engine to determine acceptable bounds for engine-internal parameters. These are not strategy parameters — they control how fast the engine processes data and how much resource it consumes. The goal is to find the point where each parameter degrades performance or stability, then set production values with comfortable margin.

| Parameter | Test range | What to measure | Failure signal |
|-----------|-----------|-----------------|----------------|
| `max_evals_per_batch` | 100 → 2,000 | Eval loop latency p50/p95, CPU% | Latency p95 > 50ms OR missed WS heartbeats |
| `efp_drift_threshold` | 0.001 → 0.020 | Urgent queue depth, eval rate, missed price moves | Queue depth sustained > 500 (too sensitive) OR profitable opps missed (too insensitive) |
| `efp_staleness_seconds` | 1.0 → 30.0 | Background eval rate, stale-book incidents | Stale-book false positives at low values; missed refreshes at high values |
| `constraint_rebuild_interval_seconds` | 60 → 1,800 | New constraint detection latency, API rate limit hits | 429s from Gamma API (too fast) OR >10 min lag behind new markets (too slow) |
| `stale_sweep_interval_seconds` | 10 → 300 | WS reconnection speed, stale book count | Stale books accumulating (too slow) OR excessive CPU (too fast) |
| `stale_asset_threshold_seconds` | 5 → 120 | False stale alerts, missed real staleness | Churning re-subscribes (too low) OR stale books used in arb math (too high) |
| `state_save_interval_seconds` | 5 → 120 | Disk I/O, state loss on simulated crash | Disk I/O > 10% (too frequent) OR >60s state loss (too infrequent) |

**Method**: Run each parameter at 5 values across its range while holding all others at baseline. Single instance, 1-hour soak per value. Automated: the test harness logs metrics to a stress-test SQLite table, then a summary script ranks values by composite score (latency + CPU + correctness).

| Task | Acceptance Criteria |
|------|-------------------|
| ⬚ **D2.5: Stress test harness** | Script that runs the engine with parameter overrides, collects latency/CPU/queue metrics for 1 hour, writes results to `data/stress_test.db`. Accepts `--param <name> --values <v1,v2,...>` args. |
| ⬚ **D2.6: Run stress tests** | All 7 parameters tested. Results table produced with recommended production values. Any parameter where current default is outside the safe zone gets updated in config.yaml. |

#### D-Part 1 (continued): Comparison

| Task | Acceptance Criteria |
|------|-------------------|
| ⬚ **D3: Comparison dashboard** | A summary page (or tab on main dashboard) showing side-by-side: P&L, capital utilisation, position count, replacement rate, arb detection rate, average position size, depth-limited trade count for all 5 instances. |

#### D-Part 2: Validation Gate

| Task | Acceptance Criteria |
|------|-------------------|
| ⬚ **D4: Run for 14 consecutive days** | All 5 instances: zero unhandled errors. Auto-recover from transient failures. |
| ⬚ **D5: Validate execution assumptions** | For every "would-execute" event across all instances: compare intended fill price with actual book state. Log `actual/expected` ratio. Target: 95%+ have ratio > `live_trading.min_profit_ratio`. |
| ⬚ **D6: Reconciliation clean** | Every reconciliation check passes on all instances. |
| ⬚ **D7: Performance baseline** | Eval latency p50 < 10ms per instance. Market coverage > 40%. |
| ⬚ **D8: Remove latest_markets.json** | Remove JSON file write from RustMarketScanner. All consumers read from SQLite or in-memory market_lookup. Delete `data/latest_markets.json` generation and `MARKETS_PATH` references. |
| ⬚ **D9: Parameter selection** | After 14 days, analyse all 5 instances across: risk-adjusted return (Sharpe-like: return / max drawdown), capital utilisation %, replacement success rate, depth-limited trade count, average hold duration vs expected. Engine parameters already determined by D2.6 stress tests. Select winning **strategy** parameter set (sizing + entry filters + replacement behaviour) for live trading. Document rationale. |
| ⬚ **D10: CTO sign-off** | CTO reviews: 14-day comparison report across all 5 instances, selected parameters with rationale, error log, reconciliation report. Written approval to proceed to live. |

---

### ⬚ Milestone E: Go Live

**Goal**: First real money trades with optimised parameters.

| Task | Acceptance Criteria |
|------|-------------------|
| ⬚ **E1: Deposit $1,000 USDC** | Funds visible in Polymarket wallet. CLOB connectivity verified. |
| ⬚ **E2: Configure winning parameters** | Apply the parameter set selected in D8 to the live instance. |
| ⬚ **E3: Switch to live mode** | Config change: `shadow_only: false`. System begins placing real orders. |
| ⬚ **E4: Supervised first trades** | Operator monitors first 3–5 live trades in real time. Verifies: orders placed correctly, fills match expectations, P&L tracking accurate. |
| ⬚ **E5: 48h supervised period** | Operator checks dashboard and WhatsApp notifications every 2–4 hours for first 48h. No intervention needed = success. |
| ⬚ **E6: Steady state** | System runs autonomously. Operator checks daily summary. Intervenes only on alerts. |

---

### ⬚ Milestone F: Stabilise & Scale (Post-Launch)

Not blocking go-live. Prioritise based on operational experience.

| Task | Priority |
|------|----------|
| Increase capital allocation (per D8 findings) | Medium — after 30 days profitable |
| Historical performance dashboard | Medium — needed for commercial pitch |
| Dashboard control panel (mode switch, config edit) | Low — CLI is fine for single operator |
| Multi-exchange support (Kalshi) | Low — separate product decision |

---

## 6. Extensibility Notes (Future Commercial)

The following should be noted but NOT implemented now:

- **Multi-tenancy**: If SaaS, each user needs isolated capital pool, positions, config. Current single-tenant architecture would need significant rework.
- **Auth**: No authentication exists. Dashboard is open on port 5556. Needs auth before any external access.
- **Per-user wallets**: Currently one private key in secrets.yaml. Multi-user would need key management.
- **Audit trail**: Current logging is operational. Commercial product needs immutable audit trail for regulatory/trust purposes.
- **API layer**: No programmatic API. Dashboard is the only interface. An API would enable integrations.
- **White-labelling**: Dashboard is functional, not branded. Commercial product needs polished UI.

**Recommendation**: Don't design for these now. The single-operator use case is different enough that premature multi-tenancy would slow everything down. Revisit after 3 months of live trading when the business case is clearer.

---

## 7. Risk Register

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| CLOB API changes break execution | Medium | High | Version-pin API client. Monitor Polymarket changelog. |
| Arb math edge case causes loss | Low | High | 100% win rate on 3 resolved arbs. Polytope completeness guards in place. |
| VPS goes down | Low | Medium | Systemd auto-restart. ZAP 3-month login reminder. |
| Polymarket blocks automated trading | Low | High | ToS review. FAK orders look like normal trading. No market manipulation. |
| Capital locked in postponed events | Medium | Low | AI postponement detector identifies and rescores. Capital velocity impact only. |
| Thin order books at scale | High | Medium | Depth gating (Milestone F). Start with $1k, not $10k. |
| 5 shadow instances overwhelm VPS | Low | Low | 8 GB RAM is ample. Shared WS connections reduce load. Monitor with `htop`. |

---

## 8. Success Criteria

The system is production-ready when ALL of these are true:

1. Single Rust binary running on VPS with zero Python dependencies
2. All hardcoded thresholds parameterised in `config.yaml` (see Parameterisation Table)
3. 14 consecutive days of 5-way shadow trading at $1,000 capital with zero unhandled errors
4. Optimal parameters selected from shadow comparison (capital_per_trade_pct, max_concurrent_positions)
5. Circuit breaker, kill switch, and WhatsApp notifications all tested and working
6. Position reconciliation passes every check for 14 days across all instances
7. Execution price validation shows 95%+ of trades would fill at > 70% of expected profit
8. CTO has reviewed comparison results and given written sign-off
9. $1,000 USDC deposited and CLOB connectivity verified
10. Documentation split into 6 focused documents (produced within A–C), all reviewed for accuracy
11. Scripts rationalised to 3 (start.sh, restart.sh, kill.sh)

---

## 9. Out of Scope

The following are explicitly NOT part of this plan:
- Multi-exchange support (Kalshi)
- Commercial features (auth, multi-tenancy, billing)
- Mobile app or mobile-friendly dashboard
- Backtesting framework
- Market-making (we only take arb opportunities, never provide liquidity)
- Email notifications (WhatsApp only per CTO decision)
- OpenClaw integration (not wanted)
