# Product Specification & Production-Readiness Plan
## Polymarket Arbitrage Trading System

**Author**: Chief Product Manager
**Audience**: Head Programmer (AI-assisted implementation via Claude)
**Date**: 2026-03-14 (Rev 2: 2026-03-17 — milestone resequencing + assessment review items)
**Status**: APPROVED by CTO 2026-03-14. Rev 2 PENDING CTO review.

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
- **Notifications**: Telegram (primary), with generic webhook fallback (WhatsApp, ntfy.sh, Discord). Not email.
- **Shadow strategy**: Run 6 parallel shadow accounts with different parameter combinations (including a fast-market instance for 5-15 min crypto price predictions) to optimise strategy before committing real money.

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
| Open positions | 10 | Optimised via shadow testing (see Milestone E) |
| Eval latency (p50) | 1–5 ms | < 10 ms |
| Market coverage | 52% (18.5k/35.8k assets) | Maintain or improve |
| Uptime | Manual restarts | 99%+ (systemd, auto-restart) |

### 2.4 Architecture (current state: v0.13.0)

**Pure Rust system** (single compiled binary since v0.10.0):
- `rust_supervisor` crate: orchestrator, CLI, config loading, systemd integration
- `rust_engine` crate: WebSocket (3-tier), order book mirror (DashMap), EFP queue (parking_lot), arb math, position manager, state persistence (rusqlite), constraint detection, market scanner, resolution validator (Anthropic API), postponement detector (Anthropic API + web search), dashboard (Axum HTTP + SSE), notifications (Telegram / webhook)

**Target state (pre go-live)**: Add LiveExecutor with Rust EIP-712 signing (Milestone B). No Python runtime dependency.

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
| 1 | **Stale header** | Version says v0.04.14; actual is v0.12.0. "Last updated" is also stale. |
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
| 13 | **Section 2.4 describes hybrid Python/Rust** | Python fully removed as of v0.10.0 (2026-03-15). Architecture section must reflect pure Rust state. |

---

## 4. Capability Audit

### 4.1 Working and Proven
- Market scanning (33k+ markets, full pagination)
- Constraint detection (mutex group finder with completeness guards)
- Arb math (Rust: direct mutex + polytope Frank-Wolfe, 4.2 µs/eval)
- WebSocket — tiered architecture (Tier A: REST scanner, Tier B: hot constraint WS pool 5-10 conns, Tier C: position + command WS 1 conn)
- Order book mirror (Rust DashMap, EFP drift detection)
- Eval pipeline (full Rust hot path, p50 = 1–5 ms)
- Position lifecycle (Rust: entry, replacement, resolution)
- State persistence (rusqlite in-memory + WAL, survives restarts)
- Dashboard (Rust axum + SSE, zero disk reads, live updates)
- AI resolution validator (Anthropic API, 1-week cache)
- AI postponement detector (web search for rescheduled events)
- Shadow mode validation (paper trades cross-checked against live books)
- Telegram/webhook notifications (rate-limited, per-event toggles, exponential backoff, hostname/instance prefix)
- Multi-instance support (6 shadow configs incl. fast-market, systemd template)
- Proactive exit (liquidation value > 1.2× resolution payout)

### 4.2 Designed but Not Implemented
- Partial fill handling (score-based unwind)
- FAK live orders
- Dashboard control panel (mode switch via UI)

### 4.3 Not Yet Designed
- Rust EIP-712 order signing (pure Rust, no py-clob-client dependency)
- Trade status pipeline (MATCHED → MINED → CONFIRMED → RETRYING → FAILED)
- Market BUY quantity semantics guard (quote vs base)
- Live P&L tracking on dashboard
- Position reconciliation (CLOB fills vs internal record)
- Venue-side reconciliation on startup (cold-start state recovery)
- Circuit breaker / auto-halt
- POL gas balance monitoring
- Disaster recovery / rollback
- Multi-shadow parameter optimisation (5 parallel accounts — see Milestone E)
- Batch order submission (multi-leg atomic entry)
- Cross-asset fill matching (negRisk synthetic NO fills)
- Formal instrument model (tick size, precision hierarchy, dynamic updates)
- Execution rate limiting (token bucket, Polymarket limits)
- Timestamp normalisation (robust parsing across API responses)
- FAK overfill handling
- UMA dispute handling

---

## 5. Production-Readiness Plan (Sequenced Milestones)

### Overview

```
✅ Milestone A: Complete Rust Port + parameterise config + docs (CHANGELOG, INCIDENT_LOG)
    |
✅ Milestone B: Build Execution Infrastructure + docs (ARCHITECTURE, ROADMAP)
    |                Complete. 27/27 tasks. 44/44 tests. Shadow-verified on VPS.
    |                No funded account required.
    |
✅ Milestone C: Safety Infrastructure + docs (OPS_RUNBOOK, USER_GUIDE) + retire PROGRESS_ROADMAP
    |                Complete. 10/10 tasks. No funded account required.
    |
⬚ Milestone D: CLOB Integration Test (small deposit: ~$50 USDC + POL gas)
    |                Prove the execution path works against the real CLOB.
    |                Place, fill, cancel, and reconcile real micro-orders.
    |
⬚ Milestone E: Shadow Validation (14 days, 6× shadow accounts, parameter optimisation)
    |                No additional funds required. Shadow trades only.
    |
⬚ Milestone F: Go Live ($1,000 USDC, supervised, VPS)
    |
⬚ Milestone G: Stabilise & Scale (post-launch)
```

### Sequencing Rationale (Rev 2)

The original spec (Rev 1) placed all execution-path tasks in Milestone B, followed by safety (C), shadow validation (D), and go-live (E). This created a sequencing error: tasks like partial fill handling, cross-asset fill matching, and position reconciliation cannot be validated without actual CLOB fills — but no funds were deposited until Milestone E.

Rev 2 fixes this by splitting execution work into two milestones:
- **Milestone B** (code-complete): everything that can be built, unit-tested, and shadow-tested without funds.
- **Milestone D** (integration test): a small deposit (~$50 USDC + POL for gas) funds real micro-orders to validate the full execution path end-to-end before the 14-day shadow validation begins.

This ensures no task is listed before its prerequisites are satisfied.

### Documentation Schedule

Each document is produced at the point in the milestone sequence when its content is settled and accurate.

| Document | Produced at | Rationale |
|----------|------------|-----------|
| **CHANGELOG.md** | End of Milestone A | Version history is stable; the Rust port is the last major architectural change before go-live. Write it while the full history is fresh. |
| **INCIDENT_LOG.md** | End of Milestone A | All incidents are historical and fully resolved. No dependency on later milestones. |
| **ARCHITECTURE.md** | End of Milestone B | By this point the Rust port is complete (A) and the execution infrastructure is built (B). The architecture is in its final state. Writing it earlier would require rewriting after B. |
| **ROADMAP.md** | End of Milestone B | With A and B complete, the roadmap can accurately reflect what's done vs remaining (C–G). |
| **OPS_RUNBOOK.md** | End of Milestone C | Safety infrastructure (circuit breaker, kill switch, Telegram, scripts) must exist before the runbook can document how to operate them. |
| **USER_GUIDE.md** | End of Milestone C | Depends on: rationalised scripts (A9), execution infrastructure (B), safety features (C). This is the last document because it must describe the complete operational system. |
| **Retire PROGRESS_ROADMAP.md** | End of Milestone C | All 6 replacement documents exist. Replace contents with a pointer to the new docs. |

---

### ✅ Milestone A: Complete Rust Port

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
| ✅ **A1: Remove paper_engine middleman** | Trading engine calls Rust PositionManager directly, no Python `paper_trading.py` in the hot path. All position operations (enter, replace, resolve, liquidate) go through Rust. |
| ✅ **A2: Port resolution validator** | Rust module makes Anthropic API calls with same prompt templates from `config/prompts.yaml`. Cache results in SQLite (same TTL). Returns same structured response. Existing test cases pass. |
| ✅ **A3: Port postponement detector** | Rust module performs web search via Anthropic API with two-attempt strategy. Same config-driven prompts. Results stored in position metadata. Rate limiting respected (60s between calls). |
| ✅ **A4: Port market scanner** | Rust module fetches all markets from Gamma API with pagination. Writes to SQLite (not JSON). Runs once at startup + periodic refresh. Handles API errors gracefully. |
| ✅ **A5: Port constraint detector** | Rust module identifies mutex groups from market data. Completeness guards read from config (not hardcoded). Results stored in Rust ConstraintStore. |
| ✅ **A6: Port config loading + parameterise all thresholds** | Rust reads `config.yaml` and `secrets.yaml` at startup. **All previously hardcoded thresholds moved to config** (see Parameterisation Table below). Hot-reload not required (restart is acceptable). |
| ✅ **A7: Port logging** | Rust `tracing` crate with `tracing-appender`. Daily rotating files (`rust_engine.YYYY-MM-DD`). Level configurable via `config.yaml`. Auto-cleanup by retention days. All `eprintln!` replaced with tracing macros. |
| ✅ **A8: Port main orchestrator** | `rust_supervisor` crate: 2.1MB binary replaces `main.py`. PID lock, SIGTERM/SIGINT handling via `nix`, subprocess monitoring with auto-restart, log cleanup. Reads config from `config.yaml`. Systemd compatible. |
| ✅ **A9: Rationalise scripts** | 3 essential scripts: `start.sh` (build if needed + P: mount + start supervisor + verify), `restart.sh` (kill + pull + rebuild + start), `kill.sh` (SIGTERM + SIGKILL + optional --cancel CLOB orders). Windows .bat files updated to delegate to .sh. VPS `setup_vps.sh` updated for Rust binary + systemd. Supervisor supports `--mode`, `--set key=value`, `--dry-run` CLI args. Legacy scripts archived. |
| ✅ **A10: Single compiled binary** | `cargo build --release` produces one binary. Deploy to VPS with config files only. No Python, no venv, no pip. |
| ✅ **A11: Create CHANGELOG.md** | All version entries (v0.01.00 through current) transferred from PROGRESS_ROADMAP.md §7. Compare with git records and update. Most recent first. keepachangelog.com format (Added/Changed/Fixed/Removed). |
| ✅ **A12: Create INCIDENT_LOG.md** | All incidents (INC-001 through INC-011) transferred. Each has: date, markets, impact, root cause, fix, status. Includes template for future incidents. |
| ✅ **A13: Independent code review** | Comprehensive review of the Rust codebase covering bugs, security, performance, style, and dependencies. Full findings and remediation plan in `Code_Review.md`. Implemented as v0.10.1 and v0.11.1 (all items addressed). |

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
| WS assets per shard (legacy flat mode) | `trading_engine.py` | 400 | `websocket.assets_per_shard` |
| WS tiered mode enabled | orchestrator | false | `websocket.use_tiered_ws` |
| WS max assets per connection | orchestrator | 450 | `websocket.max_assets_per_connection` |
| WS connection stagger | orchestrator | 150 ms | `websocket.stagger_ms` |
| Tier B max connections | orchestrator | 17 | `websocket.tier_b_max_connections` |
| Tier B hysteresis scans | orchestrator | 3 | `websocket.tier_b_hysteresis_scans` |
| Tier B consolidation threshold | orchestrator | 300 | `websocket.tier_b_consolidation_threshold` |
| Tier B top N constraints | orchestrator | 0 (no limit) | `websocket.tier_b_top_n_constraints` |
| Tier C new market buffer | orchestrator | 2.5 s | `websocket.tier_c_new_market_buffer_secs` |
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
| Trade confirmation timeout | new (B2.1) | 120 s | `live_trading.trade_confirmation_timeout_seconds` |
| Order aggression (tick offset) | new (B3.1) | `at_market` | `live_trading.order_aggression` (`passive`/`at_market`/`aggressive`) |
| **Safety** | | | |
| Circuit breaker drawdown | ✅ C1 | 10% | `safety.circuit_breaker.max_drawdown_pct` |
| Circuit breaker error count | ✅ C1 | 3 | `safety.circuit_breaker.max_consecutive_errors` |
| Circuit breaker error window | ✅ C1 | 5 min | `safety.circuit_breaker.error_window_seconds` |
| Circuit breaker API timeout | ✅ C1 | 600 s | `safety.circuit_breaker.api_timeout_seconds` |
| Gas check interval | new (C1.1) | 3600 s | `safety.gas_check_interval_seconds` |
| Min POL balance (warning) | new (C1.1) | 1.0 | `safety.min_pol_balance` |
| Critical POL balance (circuit breaker) | new (C1.1) | 0.1 | `safety.critical_pol_balance` |

**Verification**: System runs in shadow mode on VPS from single binary for 24h with zero errors. Same arb detection rate as hybrid system. Dashboard accessible. State persists across restarts. All thresholds readable from config (verified by changing one and confirming behaviour changes on restart).

---

### ✅ Milestone B: Build Execution Infrastructure

**Status**: **COMPLETE** — All 27/27 tasks done (B1–B5). 44/44 unit tests pass. Shadow mode verified on VPS. ARCHITECTURE.md and ROADMAP.md written.

**Goal**: Code-complete execution path — all modules built, unit-tested, and shadow-testable. **No funded account required.** Everything in this milestone can be verified against shadow data, dry-run mode, or unit tests comparing output against known-good reference values.

**Critical files** (all Rust after Milestone A):
- Position manager (`rust_engine/src/position.rs`)
- Arb math (`rust_engine/src/arb.rs`)
- New: `executor.rs` — live order submission
- New: `signing.rs` — EIP-712 order signing
- New: `instrument.rs` — formal instrument model
- New: `reconciliation.rs` — venue-side state comparison
- New: `monitor.rs` — system/financial metrics collection + SSE streaming
- New: `rate_limiter.rs` — token bucket for CLOB API

#### B-Part 1: Foundations (already complete)

| Task | Acceptance Criteria |
|------|-------------------|
| ✅ **B1.0: Order book depth gating** | Integrate depth check into entry/replacement flow. Gate all entries on sufficient book depth per leg. Log depth-limited trades. |
| ✅ **B1.1: Replacement chain analytics** | Track full chain history: `chain_id`, `chain_generation`, `parent_position_id` in Position struct. `get_chain_stats()` computes chain-level fees and profit. |
| ✅ **B1.2: Record retention & pruning** | Closed positions retained indefinitely. Daily pruning strips bulky metadata from records older than `closed_position_retention_days` (default 90). |
| ✅ **B1.3: Pre-trade validation** | Before any live order: check book age against `engine.max_book_staleness_secs`. Abort if depth < `live_trading.min_depth_per_leg` on any leg. Log all aborts. |
| ✅ **B3: negRisk capital calculation** | Sell arb collateral = $1.00 per unit. `capital_efficiency` and `collateral_per_unit` propagated through Opportunity to Position metadata. |

#### B-Part 2: Order Signing & Instrument Model (no funds needed — verify against reference implementation)

| Task | Acceptance Criteria |
|------|-------------------|
| ✅ **B2.0: Rust EIP-712 order signing** | Pure Rust implementation using `alloy` (or `ethers-rs`). Covers: (a) EIP-712 domain separator with Polymarket CLOB Exchange contract address and Polygon chain ID 137, (b) Order struct hashing per Polymarket typed data schema (`salt`, `maker`, `signer`, `taker`, `tokenId`, `makerAmount`, `takerAmount`, `expiration`, `nonce`, `feeRateBps`, `side`, `signatureType`), (c) ECDSA signing with private key from `secrets.yaml`, (d) signature type 0 (EOA) output. **Verified by**: construct 5 test orders, sign in both Rust and py-clob-client, assert identical signatures. No CLOB submission needed — signature comparison is sufficient. **Pre-req**: read py-clob-client `order_builder/` source and Polymarket CLOB API signing docs. |
| ✅ **B2.3: Formal instrument model** | Typed Rust struct: `tick_size`, `price_increment`, `size_increment`, `min_order_size`, `max_order_size`, `neg_risk`, `condition_id`, `token_id`, `signature_type`. Encode py-clob-client `ROUNDING_CONFIG` rules (tick size → price/size/amount decimal precision hierarchy). `neg_risk` flag determines exchange contract routing (CLOB Exchange vs Neg Risk Exchange). Instruments loaded from MarketScanner data at startup. **Pre-req**: study NT `BinaryOption` type and py-clob-client `ROUNDING_CONFIG`. |
| ✅ **B2.4: Dynamic tick size handling** | Tier C WebSocket parses `tick_size_change` events. On receipt: update instrument's tick/price/size/amount precision per `ROUNDING_CONFIG`. In-flight order validation uses current tick size at submission time. Log all changes. Recalculate precision for pending orders on affected instruments. |

#### B-Part 3: Executor, Pipeline & Rate Limiting (no funds needed — built in dry-run / shadow mode)

| Task | Acceptance Criteria |
|------|-------------------|
| ✅ **B3.0: Market BUY quantity guard** | Enforced at executor boundary: (a) Market BUY → quantity = USDC notional (quote). (b) Market SELL → quantity = token count (base). (c) Limit/FAK → quantity = token count (base). (d) Base-denominated market BUY rejected with error log, never submitted. Prevents oversized fills. Unit test covers all 4 cases. |
| ✅ **B3.1: Wire LiveExecutor (dry-run)** | CLOB `create_order()` calls for all legs of an arb. Uses B2.0 signing, B2.3 instrument model, B3.0 quantity guard. Passes `neg_risk: true` for negRisk markets. Uses FAK (Fill And Kill) order type. In `--dry-run` mode: constructs and signs orders, logs full details, but does not submit to CLOB. Handles: success, partial fill, rejection, timeout as state transitions. **Design decision**: signature type 0 (EOA). Document in ARCHITECTURE.md. **Fast-market note**: For short-lived markets (Shadow-F), FAK limit orders placed 1 tick into the book provide the best fill-rate vs price tradeoff. Market orders guarantee fill but sacrifice spread; passive limit orders risk non-fill on a 5-min market. Config key `live_trading.order_aggression` (values: `passive`, `at_market`, `aggressive`) controls tick offset per instance. |
| ✅ **B3.2: Trade status pipeline** | After MATCHED, track status via Tier C WebSocket: MATCHED → MINED → CONFIRMED → RETRYING → FAILED. (a) CONFIRMED: finalise position entry. (b) RETRYING: log warning, hold in "pending" state, exclude from replacement queue. (c) FAILED: roll back entry, restore capital, Telegram alert. (d) No update within `live_trading.trade_confirmation_timeout_seconds` (default 120s): query CLOB API, reconcile. Position not "open" until CONFIRMED. Dedup on `trade_id`. |
| ✅ **B3.3: Execution rate limiting** | Token-bucket rate limiter: 60 orders/min trading, 100 req/min public, 300 req/min auth, 3,000 req/10min global. Logs when throttled. Prevents 429 bans. **Pre-req**: study NT rate limit implementation and Polymarket API docs for current limits. |
| ✅ **B3.4: Timestamp normalisation** | Robust parsing across all API responses: ISO8601 with/without timezone, Unix seconds vs milliseconds, missing/null timestamps. Single `parse_polymarket_timestamp()` function used everywhere. **Pre-req**: study NT issue #3273 and fix for edge cases. |
| ✅ **B3.5: Error handling matrix** | Defined behaviour for: CLOB API timeout (retry once, then abort + unwind), CLOB rejection (log + skip), network failure (pause trading, alert, retry connection), insufficient balance (pause trading, alert), rate limit 429 (back off per token bucket). |
| ✅ **B3.6: Partial fill handling** | After submitting all legs: check actual fills via order status. Compute arb score of filled position. If score ≥ threshold: accept. If score < threshold: compute minimum unwind. If no acceptable partial: full unwind. Log all partial fill events. **Note**: code-complete at this point; real partial fills tested in Milestone D. |
| ✅ **B3.7: Batch order submission** | Submit all legs of a multi-leg arb as a single batch request. Reduces latency window for partial-fill exposure on 3+ leg positions. **Pre-req**: study NT PR #3506 source before building. |

#### B-Part 4: Reconciliation & Tracking (no funds needed — code-complete, tested in Milestone D)

| Task | Acceptance Criteria |
|------|-------------------|
| ✅ **B4.0: Position reconciliation (periodic)** | Every `live_trading.reconciliation_interval_seconds`: query CLOB for actual positions. Compare with internal record. Alert on discrepancy. Log results. Source of truth: CLOB API contract balances (same as NT Gamma API approach). On-chain `balanceOf` query as escalation when CLOB/internal differ by > `live_trading.reconciliation_escalation_threshold` (default $1.00). If `umaResolutionStatus == "disputed"` on a market: flag position as `disputed`, exclude from replacement queue, Telegram alert. Resume normal lifecycle when dispute resolves. **Shadow mode**: when no venue credentials are configured, reconciliation skips CLOB comparison and reports positions as checked (no false alarms). |
| ✅ **B4.1: Venue-side reconciliation on startup** | On every startup: query CLOB for contract balances and open orders. Compare against SQLite state. Report discrepancies before resuming trading. **Pre-req**: study NT `generate_order_status_reports` pattern. |
| ✅ **B4.2: Cross-asset fill matching** | When a YES fill executes on a negRisk market, Polymarket implicitly creates corresponding NO positions on complementary outcomes. Detect and reconcile these synthetic fills against internal position state. **Pre-req**: study NT PR #3345/#3357 source. |
| ✅ **B4.3: FOK/FAK overfill handling** | Detect when FAK limit orders receive more fill than expected. Adjust internal position state, log discrepancy. **Pre-req**: study NT issue #3221 and fix. |
| ✅ **B4.4: Live P&L tracking** | Dashboard shows: realised P&L (closed), unrealised P&L (open, marked to market via BookMirror best bids), fees paid, net return %. Portfolio chart plots 5 time-series: Total Value ($), Unrealized P&L ($, yellow filled), Realized P&L ($, magenta dashed), Deployed (%), Drawdown (%). Stats bar: Total Value → Deployed % → Win % → Drawdown (now/max) → Profit Factor → Recovery → Sharpe → Sortino → Avg Hold. Updated via SSE every 5s (full snapshot + delta). |

#### B-Part 5: Documentation

| Task | Acceptance Criteria |
|------|-------------------|
| ✅ **B5.0: Create ARCHITECTURE.md** | Post-Rust-port architecture, data flow, component descriptions, file structure (verified against repo), config reference (all parameterised values), signing design decision (type 0 EOA), instrument model, WS tier design, glossary. Current state only — not historical. |
| ✅ **B5.1: Create ROADMAP.md** | Phases renumbered by actual priority. Each phase: goal, status, items. Cross-references ARCHITECTURE.md. No version history or incident detail. Accurately reflects A+B as complete, C–G as remaining. |

**Verification** (as of v0.14.1): All code builds and passes 44/44 unit tests. Dry-run mode constructs, signs, and logs full order details for every "would-execute" event in shadow mode. Signature output matches py-clob-client for identical inputs. Rate limiter correctly throttles burst requests. Instrument model correctly encodes all 4 tick-size precision tiers. Reconciliation runs cleanly in shadow mode (no false alarms). Dashboard P&L charts and stats bar verified on VPS. Tier B WS connections stable at 16 with jitter v2 + biased heartbeat.

---

### ✅ Milestone C: Safety Infrastructure

**Goal**: The system can protect capital automatically and alert the operator. **No funded account required.**

| Task | Acceptance Criteria |
|------|-------------------|
| ✅ **C1: Circuit breaker** | Auto-pause trading if: (a) portfolio drawdown exceeds `safety.circuit_breaker.max_drawdown_pct` from peak, (b) `max_consecutive_errors` errors in `error_window_seconds`, (c) CLOB API unreachable for `api_timeout_seconds`. Implemented in `circuit_breaker.rs`. Peak persisted to SQLite; tripped state clears on restart. Housekeeping (state save, WS, reconciliation) continues when tripped. Telegram notification on trip. 13 unit tests. |
| ✅ **C1.1: POL gas balance monitoring** | Implemented in `gas_monitor.rs`. Queries Polygon RPC (`eth_getBalance`) every `safety.gas_monitor.check_interval_seconds` (default 3600s). Wallet address auto-derived from private key. If < `min_pol_balance` (1.0): Telegram warning. If < `critical_pol_balance` (0.1): trips circuit breaker via `GasCritical` trip reason. Balance shown in stats log line (`POL=X.XXXX`). 3 unit tests. Config under `safety.gas_monitor.*`. |
| ✅ **C2: Kill switch** | Two trigger paths: (1) `kill.sh --emergency` writes `data/kill_switch.flag`, waits 5s for orchestrator to process, then graceful shutdown. (2) Dashboard KILL SWITCH button → POST `/api/kill-switch` sets `Arc<AtomicBool>`. Orchestrator checks both each tick. Actions: (a) cancel all open CLOB orders via `executor.cancel_all_orders()` (L2 auth stub until Milestone D), (b) set mode to shadow, (c) Telegram notification. Idempotent — `kill_switch_activated` flag prevents re-trigger. |
| ✅ **C3: Notifications (Telegram)** | Implemented in `notify.rs`. Telegram backend auto-detected from webhook URL; bot token from `secrets.yaml`, chat_id from config. Generic webhook fallback for WhatsApp/ntfy/Discord. Rate limiting (10s), exponential backoff (5 failures → 5min cooldown), per-event toggles. All messages prefixed with `[hostname/instance]`. Events: startup, entry, resolution, proactive exit, error, circuit breaker, daily summary. |
| ✅ **C4: Daily P&L report** | Automated daily summary at midnight UTC. Orchestrator detects UTC day boundary in `do_periodic_tasks()`, scans positions for entries/exits/fees/net P&L in the previous day, computes capital utilisation % and drawdown from peak. Sends via `NotifyEvent::DailySummary` (Telegram). Persisted to `daily_reports` SQLite table with full JSON data payload. On startup, initialises to current day to avoid false trigger. |
| ✅ **C4.1: Seamless position close transition** | Two-part fix: (1) Server sends `closed` SSE event every 5s alongside `positions` (same tick). (2) Client buffers `positions` render until `closed` event arrives, so closed table updates before open table removes the transitioning position. 150ms fallback timeout if closed doesn't arrive. Zero visual gap on close/resolve. |
| ✅ **C4.2: Proactive near-resolution exit** | Implemented in `orchestrator::check_proactive_exits()` using `PROACTIVE_EXIT_MULTIPLIER = 1.2`. Telegram notification on each exit. |
| ✅ **C5: Create OPS_RUNBOOK.md** | `OPS_RUNBOOK.md` — 12 sections: VPS details, scripts (start/kill/restart with all flags), CLI flags + allowed --set keys, dashboard ports + SSH tunnel, log locations + rotation, circuit breaker (config, trip reasons, reset), kill switch (two paths), Telegram bot setup, POL gas top-up, backup/state persistence, config file structure + load order, monitoring checklist (daily/weekly/monthly/quarterly). |
| ✅ **C6: Create USER_GUIDE.md** | `USER_GUIDE.md` — 11 sections: product overview, prerequisites, setup from scratch (clone/build/configure), starting/stopping (3 scripts), dashboard (4 tabs + header controls), mode switching (shadow/live), multi-instance mode (5 pre-configured instances), crash recovery, safety systems overview, go-live checklist, glossary. Written for a new operator. |
| ✅ **C7: Retire PROGRESS_ROADMAP.md** | Original 1,270-line document replaced with a brief retirement note listing 7 replacement documents. Git history preserved. |

**Verification**: Simulate circuit breaker triggers (inject fake errors, simulate drawdown). Verify kill switch cancels test orders. Verify Telegram messages arrive within 60s of trigger. Verify POL balance check runs and alerts correctly. All 6 documents reviewed for accuracy and internal consistency.

---

### ⬚ Milestone D: CLOB Integration Test

**Goal**: Prove the full execution path works against the real Polymarket CLOB. Place, fill, cancel, and reconcile real micro-orders. This is a focused, short milestone (target: 1 week) that validates everything built in Milestone B before committing to the 14-day shadow validation.

**Funds required**: ~$50 USDC.e deposited to the trading wallet + ~5 POL for gas.

**Why a separate milestone**: Many Milestone B tasks (partial fill handling, cross-asset fill matching, reconciliation, batch orders, FOK overfill handling) cannot be fully validated without actual CLOB fills. Dry-run mode verifies code paths and signing, but only real micro-orders confirm end-to-end correctness. Inserting this before the 14-day shadow validation ensures execution bugs are caught early with minimal capital at risk.

| Task | Acceptance Criteria |
|------|-------------------|
| ⬚ **D1: Deposit test funds** | ~$50 USDC.e + ~5 POL deposited to trading wallet on Polygon. Wallet allowances set for Polymarket contracts (USDC + CTF + Neg Risk adapter). Balance visible on dashboard (B4.4) and via POL gas monitor (C1.1). |
| ⬚ **D2: Submit and cancel a real order** | Place a single FAK limit order on a liquid market at an off-market price (guaranteed no fill). Verify: (a) order appears in CLOB API open orders, (b) signing is correct (no rejection), (c) cancel succeeds, (d) internal state matches CLOB state. This is the smoke test for B2.0 + B3.1. |
| ⬚ **D3: Execute a real micro-fill** | Place a FAK limit order at market price on a liquid market for the minimum order size. Verify: (a) fill is received, (b) trade status pipeline (B3.2) tracks MATCHED → CONFIRMED, (c) position appears in internal state, (d) reconciliation (B4.0) matches CLOB, (e) P&L tracking (B4.4) reflects the position. |
| ⬚ **D4: Test negRisk fill** | Execute a micro-fill on a negRisk market. Verify: (a) `neg_risk: true` passed correctly, (b) cross-asset fill matching (B4.2) detects any synthetic NO positions, (c) reconciliation passes. |
| ⬚ **D5: Test multi-leg arb execution** | Execute a real 2-leg arb at minimum size. Verify: (a) batch submission (B3.7) or sequential submission succeeds on all legs, (b) partial fill handling (B3.6) functions if one leg partially fills, (c) position tracks all legs correctly. This is the first real arb. |
| ⬚ **D6: Test reconciliation cold-start** | With open test positions: restart the binary. Verify: (a) venue-side reconciliation on startup (B4.1) detects all positions, (b) internal state matches CLOB, (c) trading resumes correctly. |
| ⬚ **D7: Test circuit breaker + kill switch** | (a) Trigger circuit breaker via config override (set drawdown to 0.01%). Verify: trading pauses, Telegram fires, dashboard shows breaker state. (b) Execute kill switch. Verify: all test CLOB orders cancelled, mode set to shadow. |
| ⬚ **D8: Resolve or sell test positions** | Clean up: sell all test positions or let them resolve. Reconcile final state. Verify capital accounting is correct (initial deposit minus fees = remaining balance ± P&L). |

**Exit criteria**: All 8 tasks pass. Zero unexplained discrepancies between internal state and CLOB. All funds accounted for. Ready to proceed to 14-day shadow validation.

---

### ⬚ Milestone E: Shadow Validation (14-Day Gate)

**Goal**: Prove the complete system works without risking real money, AND determine optimal trading parameters via 6 parallel shadow accounts (including a fast-market instance for 5-15 min crypto price predictions). **No additional funds required beyond Milestone D deposit** (shadow trades are paper-only).

#### E-Part 1: Multi-Shadow Parameter Optimisation

Run 6 parallel shadow instances on the VPS (8 GB RAM supports this) with different parameter combinations. Each instance varies strategy parameters (not just sizing) to find the optimal balance across capital concentration, entry aggression, and replacement behaviour. Shadow-F is a dedicated fast-market instance targeting 5-15 minute crypto price prediction markets.

**Sizing & Concentration**

| Instance | `capital_per_trade_pct` | `max_concurrent_positions` | `max_position_size` | Shadow Capital | Strategy |
|----------|------------------------|---------------------------|---------------------|----------------|----------|
| **Shadow-A** | 5% | 40 | $500 | $1,000 | Maximum diversification |
| **Shadow-B** | 10% (current) | 20 (current) | $1,000 | $1,000 | Current baseline |
| **Shadow-C** | 20% | 15 | $1,000 | $1,000 | Moderate concentration |
| **Shadow-D** | 30% | 8 | $1,500 | $1,000 | High concentration |
| **Shadow-E** | 50% | 8 | $2,000 | $1,000 | Maximum concentration |
| **Shadow-F** | 5% | 50 | $200 | $1,000 | Fast markets (5-15 min crypto price) |

**Entry Filters**

| Instance | `min_profit_threshold` | `max_profit_threshold` | `min_resolution_time_secs` | `max_days_to_resolution` |
|----------|----------------------|----------------------|---------------------------|-------------------------|
| **Shadow-A** | 0.02 (2%) | 0.30 | 120 | 90 |
| **Shadow-B** | 0.03 (current) | 0.30 (current) | 300 (current) | 60 (current) |
| **Shadow-C** | 0.03 | 0.25 | 600 | 45 |
| **Shadow-D** | 0.04 | 0.20 | 900 | 30 |
| **Shadow-E** | 0.05 | 0.20 | 1800 | 21 |
| **Shadow-F** | 0.01 (1%) | 0.30 | 60 | 1 |

**Replacement & Risk**

| Instance | `replacement_cooldown_seconds` | `max_days_to_replacement` | `max_exposure_per_market` | Notes |
|----------|-------------------------------|--------------------------|--------------------------|-------|
| **Shadow-A** | 30 | 60 | $250 | Fast replacement, broad spread |
| **Shadow-B** | 60 (current) | 30 (current) | $500 (current) | Baseline |
| **Shadow-C** | 120 | 30 | $500 | Slower replacement |
| **Shadow-D** | 120 | 21 | $750 | Slower, tighter time window |
| **Shadow-E** | 300 | 14 | $1,000 | Very selective replacement |
| **Shadow-F** | 10 | 1 | $100 | Ultra-fast turnover, crypto price markets |

**Engine Timing (per-instance overrides)**

| Instance | `constraint_rebuild_interval_seconds` | `replacement_protection_hours` | Notes |
|----------|--------------------------------------|-------------------------------|-------|
| **Shadow-A–E** | 600 (default) | 24 (default) | Standard markets |
| **Shadow-F** | 60 | 0.1 (6 min) | Must detect + enter markets within minutes of appearing |

Shadow-A tests whether wider entry gates (lowered to 120s min resolution) and faster replacement capture more value. Shadow-D/E test whether stricter filters and larger positions outperform despite fewer trades. Shadow-B remains the unchanged baseline. Shadow-F tests an entirely different market class: short-lived crypto price predictions that resolve in 5-15 minutes.

Each instance runs independently with its own SQLite database and log files. Dashboard shows a comparison view of all 5.

**WebSocket connection budget**: Single-instance default is `tier_b_max_connections: 16` (raised from 15 — jitter v2 + biased heartbeat provides sufficient spread). With 6 instances sharing one IP, total Tier B connections must stay within Polymarket's per-IP limit (~20-30, per INC-011). Shadow-A–E set `websocket.tier_b_max_connections: 2`; Shadow-F sets `websocket.tier_b_max_connections: 1` (crypto price is a smaller market subset). Total: 5×2 + 1×1 = 11 Tier B + 6 Tier C = 17 total, safely within budget. Stagger instance startup by 30s to avoid connection burst.

**Extensibility**: The multi-instance system is not limited to 6 instances. Adding more requires only: (1) a new `config/instances/{name}.yaml` overlay file, (2) `systemctl start prediction-trader@{name}`. Any config key can be overridden per-instance.

| Task | Acceptance Criteria |
|------|-------------------|
| ✅ **E1: Multi-instance support** | Single binary accepts `--instance <n>` flag. Each instance auto-configures: separate SQLite DB, log directory, PID file, dashboard port. Instance config overlays loaded from `config/instances/{name}.yaml`. |
| ✅ **E2: Deploy 6 instances to VPS** | Six instance config overlays (A–F). Systemd template unit. Management script. Resource limits: 1500M memory, 50% CPU per instance. Shadow-F uses fast-market overrides (60s constraint rebuild, 60s min resolution, 10s replacement cooldown). |

#### E-Part 1.5: Engine Stress Testing

Before building the comparison dashboard, stress-test the engine to determine acceptable bounds for engine-internal parameters.

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
| ⬚ **E2.5: Stress test harness** | Script that runs the engine with parameter overrides, collects latency/CPU/queue metrics for 1 hour, writes results to `data/stress_test.db`. Accepts `--param <n> --values <v1,v2,...>` args. |
| ⬚ **E2.6: Run stress tests** | All 7 parameters tested. Results table produced with recommended production values. Any parameter where current default is outside the safe zone gets updated in config.yaml. |

#### E-Part 1 (continued): Comparison

| Task | Acceptance Criteria |
|------|-------------------|
| ⬚ **E3: Comparison dashboard** | A summary page (or tab on main dashboard) showing side-by-side: P&L, capital utilisation, position count, replacement rate, arb detection rate, average position size, depth-limited trade count for all 6 instances. Shadow-F additionally shows: average hold duration, turnover rate, and markets-per-hour. |

#### E-Part 2: Validation Gate

| Task | Acceptance Criteria |
|------|-------------------|
| ⬚ **E4: Run for 14 consecutive days** | All 6 instances: zero unhandled errors. Auto-recover from transient failures. |
| ⬚ **E5: Validate execution assumptions** | For every "would-execute" event across all instances: compare intended fill price with actual book state. Log `actual/expected` ratio. Target: 95%+ have ratio > `live_trading.min_profit_ratio`. |
| ⬚ **E6: Reconciliation clean** | Every reconciliation check passes on all instances. |
| ⬚ **E7: Performance baseline** | Eval latency p50 < 10ms per instance. Market coverage > 40%. |
| ✅ **E8: Remove latest_markets.json** | Removed `write_json_file()` and `json_output_path` field. All consumers read from SQLite. |
| ⬚ **E9: Parameter selection** | After 14 days, analyse all 6 instances across: risk-adjusted return (Sharpe-like: return / max drawdown), capital utilisation %, replacement success rate, depth-limited trade count, average hold duration vs expected. Engine parameters already determined by E2.6 stress tests. Select winning **strategy** parameter set(s) for live trading — may select different params for fast vs standard markets. Document rationale. |
| ⬚ **E10: CTO sign-off** | CTO reviews: 14-day comparison report across all 6 instances, selected parameters with rationale, error log, reconciliation report. Written approval to proceed to live. |

---

### ⬚ Milestone F: Go Live

**Goal**: First real money trades with optimised parameters.

| Task | Acceptance Criteria |
|------|-------------------|
| ⬚ **F1: Deposit $1,000 USDC** | Additional funds deposited (total = ~$1,050 including D1 deposit). Top up POL gas if needed. Balance verified on dashboard and via CLOB API. |
| ⬚ **F2: Configure winning parameters** | Apply the parameter set selected in E9 to the live instance. |
| ⬚ **F3: Switch to live mode** | Config change: `shadow_only: false`. System begins placing real orders. |
| ⬚ **F4: Supervised first trades** | Operator monitors first 3–5 live trades in real time. Verifies: orders placed correctly, fills match expectations, P&L tracking accurate, trade status pipeline confirms all fills. |
| ⬚ **F5: 48h supervised period** | Operator checks dashboard and Telegram notifications every 2–4 hours for first 48h. No intervention needed = success. |
| ⬚ **F6: Steady state** | System runs autonomously. Operator checks daily summary. Intervenes only on alerts. |

---

### ⬚ Milestone G: Stabilise & Scale (Post-Launch)

Not blocking go-live. Prioritise based on operational experience.

| Task | Priority |
|------|----------|
| Increase capital allocation (per E9 findings) | Medium — after 30 days profitable |
| Automated POL gas bridge top-up | Medium — auto-bridge USDC→POL when balance < threshold, eliminating manual top-ups |
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
| Thin order books at scale | High | Medium | Depth gating (B1.0). Start with $1k, not $10k. |
| 6 shadow instances overwhelm VPS | Low | Low | 8 GB RAM is ample. Per-instance resource limits. Monitor with `htop`. |
| EIP-712 signing incompatible with CLOB | Low | Critical | B2.0: verify against py-clob-client output before any live use. Use Polymarket sandbox for integration test (Milestone D). |
| Market BUY with base quantity causes oversized fill | Low | High | B3.0: explicit guard at executor boundary rejects base-denominated market BUYs. |
| POL gas exhaustion blocks settlement | Low | High | C1.1: gas balance monitoring + circuit breaker trigger. Maintain ≥ 5 POL buffer. |
| 6 shadow instances exceed per-IP WS connection limit | Medium | Medium | Shadow-A–E: 2 Tier B each, Shadow-F: 1 Tier B (11 Tier B + 6 Tier C = 17 total). Stagger startup by 30s. |
| Trade CONFIRMED after RETRYING creates duplicate position | Low | Medium | B3.2: position state machine prevents double-entry. Dedup on `trade_id`. |
| Tick size change mid-position invalidates order precision | Low | Medium | B2.4: dynamic tick size handling via WS event. Recalculate precision for pending orders. |
| UMA dispute locks capital with volatile prices | Low | Medium | B4.0: flag disputed positions, exclude from replacement, alert operator. |

---

## 8. Success Criteria

The system is production-ready when ALL of these are true:

1. Single Rust binary running on VPS with zero Python dependencies
2. All hardcoded thresholds parameterised in `config.yaml` (see Parameterisation Table)
3. EIP-712 order signing verified against py-clob-client reference (Milestone B)
4. CLOB integration test passed with real micro-orders — zero unexplained discrepancies (Milestone D)
5. 14 consecutive days of 6-way shadow trading at $1,000 capital with zero unhandled errors (Milestone E)
6. Optimal parameters selected from shadow comparison (capital_per_trade_pct, max_concurrent_positions)
7. Circuit breaker, kill switch, POL gas monitoring, and Telegram notifications all tested and working
8. Position reconciliation passes every check for 14 days across all instances
9. Execution price validation shows 95%+ of trades would fill at > 70% of expected profit
10. CTO has reviewed comparison results and given written sign-off
11. ~$1,050 USDC + POL deposited and CLOB connectivity verified
12. Documentation split into 6 focused documents (produced within A–C), all reviewed for accuracy
13. Scripts rationalised to 3 (start.sh, restart.sh, kill.sh)

---

## 9. Out of Scope

The following are explicitly NOT part of this plan:
- Multi-exchange support (Kalshi)
- Commercial features (auth, multi-tenancy, billing)
- Mobile app or mobile-friendly dashboard
- Backtesting framework (Polymarket does not expose historical book depth; PT's EFP-dependent strategy cannot be meaningfully backtested against trade-only data; small-capital live validation via Milestones D+E is the correct approach)
- Market-making (we only take arb opportunities, never provide liquidity)
- Email notifications (Telegram only per CTO decision)
- OpenClaw integration (not wanted)
