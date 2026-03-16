# Changelog

All notable changes to the Prediction Market Arbitrage System are documented in this file.

Format based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versioning: `vMAJOR.MINOR.PATCH` with zero-padded two-digit minor and patch.

---

## [0.11.0] — 2026-03-16 — Notifications, Multi-Instance, Python Retirement

### Added
- **C3**: WhatsApp notification module (`notify.rs`) — rate-limited (10s), exponential backoff (5 failures → 5min cooldown), per-event toggles, generic HTTP webhook POST
- **C3**: Notifier wired into orchestrator for entry, WS resolution, API resolution, and proactive exit events
- **C4.2**: Proactive exit — sells position when liquidation value > 1.2× expected resolution payout
- **D1**: Multi-instance support — `--instance <name>` auto-configures DB, logs, PID file, and dashboard port
- **D1**: Instance config overlays loaded from `config/instances/{name}.yaml` with dot-notation flattening
- **D2**: `scripts/deploy_shadows.sh` — systemd management for 5 shadow instances (install/start/stop/restart/status/logs)
- **D2**: `scripts/systemd/prediction-trader@.service` — template unit with MemoryMax=1500M, CPUQuota=50%
- **D2**: 5 shadow instance configs (`shadow-a` through `shadow-e`) with varied strategy parameters
- `scripts/watch_trader.sh` — color-coded real-time log monitor (entries, resolutions, exits, errors, circuit breaker)
- `notifications` section in `config.yaml` with webhook URL, API key, phone number, per-event toggles

### Changed
- **D8**: Removed `latest_markets.json` generation — scanner uses SQLite-only storage, saves disk space pre-VPS migration

### Removed
- All Python source files archived — `main.py`, `trading/`, `utilities/`, `arbitrage_math/`, `constraint_detection/`, `market_data/`, `scripts/*.py`
- Unused JSON files moved to `archive/` or deleted (large market snapshots)

### Fixed
- **INC-008**: UMA resolution status — use `umaResolutionStatus` field instead of inferring from prices
- **INC-009**: Periodic API resolution polling restored + `outcomePrices` string parsing
- **INC-010**: Initial capital defaulting to $1000 instead of reading from config

### Performance
- **L1**: Event-driven wake — `Condvar` replaces unconditional 50ms sleep; WS handlers signal on urgent push
- **L2**: Batched PositionManager locks — `get_held_ids()` single-lock accessor replaces 4–5 separate lock acquisitions per tick
- **L3**: Dashboard clone-then-release — position data cloned under lock, JSON built after `drop(pm)`
- **L4**: Async disk I/O — state saves spawned in background thread, no longer block the tick loop

---

## [0.10.1] — 2026-03-15 — Code Review: Bugs, Performance, Security & Cleanup

### Fixed
- **B1**: OrderedFloat NaN safety — `partial_cmp().unwrap_or()` → `total_cmp()`
- **B2+B4**: Double-Mutex on PositionManager — removed redundant inner `Mutex<PositionManagerInner>` wrapper
- **B3**: Unbounded WS event accumulation — resolved events vector now always drained after processing
- **B5**: WS resolution field swap — `resolve_by_ws_events` was receiving `(asset_id, condition_id)` instead of `(condition_id, asset_id)`
- **B6**: Held-set filtering in `try_enter_or_replace` — pre-built `HashSet` of held condition/market IDs passed to validation
- **B7**: Replacement assertion — `debug_assert!` verifying replacement target exists before entering new position
- **B8**: WAL pragma removed from in-memory SQLite — WAL mode is invalid for `:memory:` databases (state, resolution, postponement, scanner)

### Security
- **S3**: CLI `--set` allowlist — only permitted config keys can be overridden via command line
- **S4**: Dashboard binds to `127.0.0.1` instead of `0.0.0.0`
- **S5**: Generic user-agent header on HTTP client

### Performance
- **P1**: Typed position accessors — dashboard, orchestrator, and API resolution checks use `&Position` directly instead of JSON serialize/parse round-trips
- **P2**: `last_efp` timestamp moved into `OrderBook` to avoid repeated `Instant::now()` calls
- **P4**: `EvalQueue::push` accepts `now` parameter to batch timestamp lookups
- **P6**: Shared `reqwest::blocking::Client` on TradingEngine instead of creating per-request clients

### Refactored
- **I1**: `classify_category` extracted to `types.rs` — deduplicated from dashboard.rs and orchestrator.rs
- **I2**: `CachedSqliteDB` generic struct — ~200 lines of duplicated in-memory + disk backup boilerplate eliminated across 4 modules
- **I4**: Config values cached on `Orchestrator` struct instead of re-reading HashMap on every tick
- **I5**: `build_leg_json` helper in dashboard.rs replaces duplicated leg-building code
- **I6**: Removed unused `msg_count` field from `BookMirror`
- **I7**: Removed dead `deployed_dollars` variable from dashboard.rs
- **I8**: Held-set reuse in `try_enter_or_replace` — single computation shared across validation and replacement

### Style & Maintainability
- **ST1**: Python-era comments updated throughout (ws.rs, position.rs, eval.rs, queue.rs, state.rs)
- **ST3**: `#[must_use]` on key return types (Opportunity, ScanResult, PositionManager, ConstraintStore)
- **ST5**: `len()` → `live_count()` on BookMirror for semantic clarity
- **ST6**: Named constants for magic numbers (MIN_PRICE_SUM_THRESHOLD, MAX_POLYTOPE_MARKETS, SYNTHETIC_DEPTH)
- **ST7**: `Default` derive on ConstraintStore, EvalQueue
- **D1**: WS raw message logging downgraded from `debug!` → `trace!`
- **D2**: WS pong logging downgraded to `trace!`
- **D3**: Postponement cache-hit log removed (noisy)
- **D4**: Dashboard SSE comment-only keepalive to avoid empty-line disconnects

### Dependencies
- **DEP1**: `serde_yaml` → `serde_yaml_ng` (serde_yaml deprecated)
- **DEP3**: `rusqlite` 0.33 → 0.38

### Hardware
- **H1**: Tokio worker threads set to `available_parallelism()` instead of hardcoded 2

### Validator
- Temperature unit mismatch instruction added to resolution validation prompt — unit differences (°C/°F, km/miles) no longer trigger false rejections

---

## [0.10.0] — 2026-03-15 — A10: Single Compiled Binary

### Added
- Cargo workspace with two members: `rust_engine` (lib) and `rust_supervisor` (binary)
- `rust_supervisor/src/orchestrator.rs` — full port of Python `trading_engine.py` (~1058 lines) into Rust
- Arc-wrapped shared state: BookMirror, EvalQueue, PositionManager, ConstraintStore
- 50ms synchronous tick event loop replacing Python asyncio
- Axum HTTP + SSE dashboard served from the binary
- Anthropic API integration for resolution validation and postponement detection
- Opportunity ranking: `profit_pct / effective_hours` with P95 delay table and category classification
- Platform-conditional compilation: `nix` for Unix signals, `ctrlc` for Windows
- CLI flags: `--port`, `--set key=value`, `--no-pid-lock`, `--workspace`, `--mode`, `--dry-run`, `--log-level`
- Configurable state DB path (`state.db_path` in config or `--set` override)

### Changed
- `rust_engine` converted from `cdylib` (PyO3) to standard `rlib` — no Python dependency
- All PyO3 decorators (`#[pyclass]`, `#[pymethods]`, `#[pymodule]`) stripped from lib.rs, scanner.rs, resolution.rs, postponement.rs
- `RustMarketScanner` → `MarketScanner`, `RustResolutionValidator` → `ResolutionValidator`, `RustPostponementDetector` → `PostponementDetector`
- `scripts/start.sh` — workspace cargo build replaces venv/maturin build
- `scripts/restart.sh` — single `cargo build --release` replaces separate supervisor + maturin builds
- `scripts/kill.sh` — simplified process grep, removed Python state snapshot

### Removed
- All Python runtime dependency — no venv, no pip, no Python interpreter required
- PyO3 crate dependency from rust_engine
- `--python` CLI argument from supervisor

---

## [0.05.02] — 2026-03-14 — A3: Port Postponement Detector to Rust

### Added
- `rust_engine/src/postponement.rs` — Anthropic API with `web_search_20250305` tool
- Two-attempt retry strategy with context injection for rescheduled event detection
- Rate limiting (60s between API calls) via `Mutex<Instant>`
- In-memory SQLite cache with disk backup, date buffer + midnight rounding

### Changed
- `trading_engine.py`: `self.rust_pp.check()` replaces Python `check_postponement()`
- Replacement scoring reads `self.rust_pp.load_cache(pid)` instead of broken `pos_meta.get('postponement')`

### Fixed
- Postponement results were never stored since A1 (`pos.metadata` referenced undefined `pos` variable)

---

## [0.05.01] — 2026-03-14 — A2: Port Resolution Validator to Rust

### Added
- `rust_engine/src/resolution.rs` — `reqwest::blocking` HTTP client for Gamma API + Anthropic Messages API
- In-memory SQLite cache with disk backup (`StateDB` pattern, `mirror_to_disk()`)
- `serde_yaml` for reading `config/prompts.yaml` + `config/config.yaml` directly in Rust

### Changed
- `trading_engine.py`: `self.rust_rv.validate()` replaces Python `get_full_validation()`
- `_save_state()`: calls `self.rust_rv.mirror_to_disk()` for cache persistence

### Fixed
- `_load_constraints_into_rust()` called `_pm_capital()` before `rust_pm` wired — used `paper_engine.current_capital` fallback

---

## [0.05.00] — 2026-03-14 — A1: Remove paper_engine Middleman

### Changed
- All position ops now go through Rust PM directly (`self.rust_pm = self.rust_ws` wired after init)
- `_pm_enter()`: early-return pattern, added missing `current_no_prices` parameter
- `_validate_opportunity()`: uses Rust PM `_pm_held_cids()` / `_pm_held_mids()`
- All `paper_engine.current_capital` / `paper_engine.open_positions` in hot path replaced with Rust PM calls
- Monitoring loop uses Rust `check_resolutions()` + `close_on_resolution()`
- `_check_postponements()` iterates Rust PM positions via `get_open_positions_json()`

### Added
- `_build_market_prices()`: constructs price dict for Rust `check_resolutions()`
- Rust PM first-time operation monitor batch file

### Fixed
- State loading no longer gated on `.json` file existence (pre-existing bug since v0.03.00 — see INC-007)
- PM init moved after `rust_ws.start()` (was unreachable before rust_ws creation)
- `open_count()` → `pm_open_count()` (PyO3 method name mismatch)
- Stripped trailing null bytes from `trading_engine.py` (Windows FS artifact)

---

## [0.04.24] — 2026-03-13 — State Persistence + Dashboard Post-Validation

### Changed
- State persistence improvements and dashboard post-validation display

---

## [0.04.23] — 2026-03-13 — AI Pre-filter + Replacement Iteration

### Added
- AI pre-filter for replacement scoring
- TODO admin tab on dashboard

---

## [0.04.22] — 2026-03-13 — Fix AI Validator False Rejections

### Fixed
- AI validator false rejections on sports markets with ambiguous resolution language

---

## [0.04.21] — 2026-03-13 — Shares, Resolution Dates, State Persistence

### Fixed
- Share calculation accuracy
- Resolution date display
- State persistence edge cases

---

## [0.04.20] — 2026-03-12 — Rust Axum Dashboard

### Added
- `rust_engine/src/dashboard.rs` — Rust axum HTTP + SSE dashboard replacing `dashboard_server.py`
- Zero disk reads — all data served from in-memory state
- Single process serves dashboard alongside trading engine

### Removed
- Python `dashboard_server.py` superseded by Rust implementation

---

## [0.04.14] — 2026-03-12 — Rust Eval Pipeline Wired (Phase 8 P4c Complete)

### Added
- `_load_constraints_into_rust()`: builds constraint+market data, loads into Rust evaluator
- Full pipeline WS → book → queue → arb math runs in Rust, returns opportunity dicts directly

### Changed
- `_process_pending_evals` replaced with single `evaluate_batch()` call
- Capital/thresholds updated on every eval batch

### Performance
- p50 = 1–5ms eval latency (was 24ms with Rust WS + Python eval, was 35ms all-Python)
- 1,365 arb candidates found in 2 hours of production running

---

## [0.04.13] — 2026-03-12 — Arb Math Merged into rust_engine (Phase 8 P4c Scaffold)

### Added
- `rust_engine/src/arb.rs` — pure Rust arb math: `check_mutex_arb()`, `polytope_arb()`, `build_scenarios()`
- `rust_engine/src/eval.rs` — batch evaluator: `ConstraintStore`, `evaluate_batch()`
- `set_constraints()` and `set_eval_config()` PyO3 bindings

### Fixed
- DB position sync: 10 positions now match between JSON and SQLite (was 8 due to migration gap)

---

## [0.04.12] — 2026-03-12 — Rust State Integration (Phase 8 P4b Wired)

### Added
- `utilities/rust_state_adapter.py` — Python adapter wrapping `RustStateDB`

### Changed
- `paper_trading.py` tries Rust adapter first, falls back to Python `StateStore`

### Fixed
- Schema match: Rust uses `scalars` table with `REAL` values matching Python DB

---

## [0.04.11] — 2026-03-12 — Rust SQLite State (Phase 8 P4b)

### Added
- `rust_engine/src/state.rs` — `RustStateDB` with in-memory SQLite + `rusqlite::backup` disk mirror
- `RustStateDB` PyO3 class: scalar CRUD, position CRUD, bulk save, `mirror_to_disk()` with GIL-free backup

### Performance
- Rust WS latency (1001 samples, 8+ hrs): p50=24ms, p95=49ms, reconnect spikes eliminated
- Live WS coverage doubled: 18,590/35,807 (52%) vs Python 8,500/35k (24%)

---

## [0.04.10] — 2026-03-12 — Dashboard Polish + Rust WS Integration (Phase 8 P4a)

### Added
- `rust_engine/` crate integrated: tokio-tungstenite WS, parking_lot eval queue, DashMap book mirror
- Phase 8 P4a complete: Rust handles WS, queue, book mirror; Python retains arb math + position lifecycle

### Fixed
- System section reads from execution_state (not stale engine status file)
- Shadow tab log parsing reads correct log filenames
- Collapse All button hidden by default; expanded rows persist across SSE updates

### Changed
- Past opportunities (score < 0) no longer displayed

---

## [0.04.09] — 2026-03-11 — Full SSE Dynamic Dashboard (Zero-Refresh)

### Changed
- Complete rewrite of `dashboard_server.py` — static HTML shell + typed SSE events
- All data sections update live: stats (5s), positions (5s), opportunities (15s), system (10s), closed (60s), shadow (30s), live (30s)
- All table rendering done client-side in JavaScript
- USDC balance cached for 60s

---

## [0.04.08] — 2026-03-11 — AI Postponement Detection

### Added
- `postponement_detector.py` — AI web search for rescheduled events via Anthropic API
- Two-attempt strategy: retry with context injection and alternative search strategies
- `config/prompts.yaml` — all AI prompt templates extracted from code
- `config.yaml → ai:` section for centralised AI config

### Changed
- `resolution_validator.py` reads model/prompt from config
- Replacement scoring uses AI-detected effective dates, overrides raw end_date

---

## [0.04.07] — 2026-03-11 — Code Cleanup, Paper Retirement

### Changed
- Scanner runs once at startup (blocking, 120s timeout), not as persistent process
- Mode display simplified: `shadow` or `live` (no more `paper`)

### Removed
- Paper trading mode retired — shadow is minimum operating mode
- Legacy runners archived: `layer2_runner.py`, `layer3_runner.py`, `layer4_runner.py`
- `paper_trading:` config section

### Performance
- Post-P3 stabilised: p50=35ms, p90=195ms (steady-state, bg=0)

---

## [0.04.06] — 2026-03-10 — Batch EFP + Dirty-Asset Buffering (Phase 8 P3)

### Added
- `batch_effective_fill_prices()` in Rust: batch EFP for all dirty assets in one PyO3 call
- `_process_dirty_assets()`: batch processes buffered assets at start of each eval loop
- Stale-asset WS re-subscribe sweep every 60s (replaces REST fallback)

### Changed
- WS callbacks buffer `asset_id` into set (1 Python op) instead of per-event queue processing (~10 ops)

---

## [0.04.05] — 2026-03-10 — Rust Polytope Reintroduction (Phase 8 P2)

### Added
- `polytope_arb()` in Rust: ~181µs vs Python CVXPY ~80ms (440× speedup)
- `build_scenarios()` in Rust: generates valid outcome matrices for mutex, complementary, logical_implication

### Changed
- Polytope runs for mutex constraints where direct check found no arb
- Skipped Bregman KL pre-filter — Rust FW fast enough to run unconditionally

---

## [0.04.04] — 2026-03-10 — Dashboard SSE + Exec Control Removal

### Changed
- Dashboard AJAX polling replaced with Server-Sent Events (`/stream` endpoint, 5s push)
- Dashboard reads from SQLite state DB (primary) with JSON fallback

### Removed
- Execution control server, client, and `exec_claim.sh` — eliminated 12.9ms/iteration overhead
- Control Panel tab
- 136-line dead code section (merge artifact)

### Performance
- Post-P0/P1 (697 samples): steady-state p50 = 19–167ms (was 2–6s)

---

## [0.04.03] — 2026-03-10 — Latency Bottleneck Analysis + P0/P1 Fixes

### Added
- `state_db.py`: SQLite read-only access for dashboard
- Phase 8 latency optimisation plan (P0–P4)

### Fixed
- All P0 fixes: event-based wake, exec lock caching (30s), batch 500, no-indent JSON
- All P1 fixes: SQLite in-memory + WAL persistence, incremental position updates

### Performance
- GIL contention identified as 80% of wall time (not arb math at 8%)
- Eval throughput: ~17/s → ~100/s → ~500/s across P0/P1

---

## [0.04.02] — 2026-03-09 — EFP Queue Metric + negRisk Tagging

### Added
- Effective Fill Price (EFP) as 2D queue metric: VWAP at trade size
- Priority queue: urgent (EFP drift > $0.005) first, background (>5s stale) fills remainder
- Real latency instrumentation: p50/p95/max from queue time → eval time
- negRisk metadata tag and CLOB order flag

---

## [0.04.01] — 2026-03-09 — Threaded Arb Eval + WS Stability

### Added
- `ThreadPoolExecutor` (2 workers) for CPU-bound arb evaluation
- `MAX_EVALS_PER_BATCH = 100` (raised to 500 in v0.04.03)

### Changed
- `ASSETS_PER_CONNECTION` reduced from 4,000 → 2,000 (smaller shards)

### Fixed
- Zero WS disconnects over 8+ minutes (was 8+ disconnects per 8 min)

---

## [0.04.00] — 2026-03-08 — Event-Driven Trading Engine

### Added
- `trading_engine.py` — single async event-driven process replacing L2+L3+L4
- Bid/ask spread-aware arb math using actual ask prices (not midpoints)
- `has_live_prices()` gate — constraints only evaluated when all markets have live WS data
- WS sharded connection pool (N connections × 2,000 assets each)

### Changed
- Architecture becomes two-process: Market Scanner + Trading Engine
- Legacy layer runners kept as reference only

---

## [0.03.06] — 2026-03-08 — WebSocket Integration (Phase 6)

### Added
- `websocket_manager.py` — persistent WS connections, local order book mirror, callback system, auto-reconnect
- WS → L3 price bridge (`ws_prices.json`): actual No prices from WS book
- Resolved market pruning via `market_resolved` WS event

---

## [0.03.05] — 2026-03-06 — Dynamic Resolution Delay Model + VPS Deployment

### Added
- Resolution delay scoring: `effective_hours = raw_hours + P95_category_delay + volume_penalty`
- Dynamic P95 table loaded from `data/resolution_delay_p95.json`
- `scripts/debug/update_delay_table.py` — weekly updater

### Changed
- Deployed to ZAP-Hosting Lifetime VPS (193.23.127.99), $100 fresh shadow capital, systemd auto-restart
- Harvested 512,894 resolved markets from Gamma API for delay analysis

---

## [0.03.04] — 2026-03-04 — Sell Arb Payout Formula Fix

### Fixed
- Critical payout error for `mutex_sell_all` positions. Sell arb buys NO on every leg; when outcome resolves, the winning leg's NO *loses* and all other legs' NO bets *win*. Corrected formula: `payout = sum(bet_amount_k / (1 − entry_price_k))` for all non-winning legs. Previously used buy-arb formula, severely understating sell arb P&L.

---

## [0.03.03] — 2026-03-04 — Replacement Filter

### Added
- `max_days_to_replacement: 30` — replacement candidates must resolve within 30 days (stricter than 60-day entry filter)

### Fixed
- `restart.sh` awk syntax error; `>` → `>>` to append main.log on restart

---

## [0.03.02] — 2026-03-03 — Replacement Loop Fix + Fee Config

### Fixed
- Replacement loop: same opportunity could liquidate multiple positions per round. Added `used_opp_cids` set.
- Fee config: `paper_trading.py` was reading `polymarket_taker_fee` from wrong config path

### Changed
- Moved 57 investigation scripts to `scripts/debug/` (gitignored)

---

## [0.03.01] — 2026-03-03 — Replacement Protection + Validator Verification

### Added
- 24h replacement protection: positions with AI-validated resolution < 24h away are immune from replacement

### Fixed
- Replacement scoring uses AI-validated resolution date from cache (not raw API `end_date`)

---

## [0.03.00] — 2026-03-03 — Resolution Safety

### Added
- `_check_group_resolved()` — all markets in group must show price → 1.0 before closing
- `resolution_validator.py` — Anthropic API call to validate true resolution dates
- `max_days_to_resolution: 60` entry filter
- L3 polytope path mutex completeness guard (sum < 0.90)

### Changed
- L1 now paginates fully (33k+ markets, removed 10k cap)

### Removed
- `_expire_position()` — positions no longer close by time; capital locked until markets resolve

### Fixed
- Japan unemployment positions (INC-001) and Somaliland positions (INC-002) cleaned

---

## [0.02.00] — 2026-02-28 — Dashboard and Scripts

### Added
- Dashboard with tabs: Paper / Shadow / Live / Control Panel
- Score column (`profit_pct / hours × 10,000`)
- Mode badge with colour coding
- Git repository established

### Changed
- Script rationalisation: 21 → 8 scripts; naming standardised

---

## [0.01.00] — 2026-02-17 — Initial System

### Added
- Four-layer pipeline (L1 scan → L2 detect → L3 evaluate → L4 execute) with supervisor (`main.py`)
- Paper trading engine with full position lifecycle
- CVXPY LP arbitrage detection
- Multi-machine execution control
- CLOB integration and shadow mode
