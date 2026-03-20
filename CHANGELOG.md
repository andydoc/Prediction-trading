# Changelog

All notable changes to the Prediction Market Arbitrage System are documented in this file.

Format based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versioning: `vMAJOR.MINOR.PATCH` with zero-padded two-digit minor and patch.

---

## [0.15.0] — 2026-03-20 — Milestone D: CLOB Integration Tests

### Added
- **CLOB integration test harness** (D1-D8): 8 acceptance tests against real Polymarket CLOB. Test binary `clob-test` with CLI flags (--workspace, --skip-deposit-check, --skip-tests, --dry-run, --resume-from).
- **WS User Channel client** (ws_user.rs): Authenticated WebSocket connection to Polymarket user channel for real-time trade and order events. Fill tracking via CONFIRMED trade events.
- **Fill tracker** (fill_tracker.rs): Confirms order fills via WS user channel, enters positions in engine PositionManager. REST /positions fallback.
- **`query_clob_positions()`** now live: HMAC-authenticated GET /positions REST endpoint for reconciliation (was stub returning empty).
- **`--skip-tests` CLI flag**: Comma-separated test IDs to skip (e.g., `--skip-tests D2,D3,D4,D7`). Skipped tests auto-PASS.
- **Position serialization in D6 checkpoint**: Full Position objects serialized to JSON for cold-start recovery.

### Fixed
- **D5 false PASS**: D6 trigger fired before D5 ran when D3+D4 produced 2 position IDs. Now --skip-tests avoids the flow conflict.
- **D8 false PASS**: Trivial pass when 0 positions open. Now fails explicitly if no positions exist at D8 start.
- **Reconciliation with real CLOB credentials**: `reconcile_startup_with_auth()` and `reconcile_periodic_with_auth()` accept ClobAuth for live position queries. Legacy no-auth wrappers preserved for shadow mode.

---

## [0.14.10] — 2026-03-20 — CLOB Order Execution

### Added
- **Complete CLOB order execution**: L1/L2 authentication, EIP-712 signing, amount precision, GTC order type for test harness.
- **Auto-derive CLOB API credentials** from wallet private key if not in secrets.yaml.
- **Instrument registration** from Gamma API market discovery (condition_id, token_ids, tick_size).

### Fixed
- **clobTokenIds deserialization**: Gamma API returns stringified JSON array, not raw array.
- **tickSize** added to CLOB order payload (required field).

---

## [0.14.9] — 2026-03-19 — L1/L2 CLOB API Authentication

### Added
- **L1 auth** (EIP-712 ClobAuth signing) for /auth/* endpoints.
- **L2 auth** (HMAC-SHA256) for trading endpoints (/order, /cancel-all, /positions).
- `ClobAuth` struct with `build_headers()` for any authenticated request.

---

## [0.14.8] — 2026-03-18 — Strategies Tab + Audit Fixes

### Added
- **Strategies dashboard tab** with 6 virtual portfolios (Shadow A-F). Each portfolio runs independent eval with its own parameter set.
- **Shadow-F configuration** added to multi-instance grid.

### Fixed
- **Audit v2/v3/v4 fixes**: 108 findings addressed across 17 files (B4 reconciliation, P1 config loading, P4/P8 performance, PY2/PY4 Python interop, safe date slicing).
- **Telegram 400 error**: Removed parse_mode HTML from notifications.

---

## [0.14.7] — 2026-03-18 — C4: Daily P&L Report + C4.1: Seamless Close

### Added
- **Daily P&L report** (C4): Automated summary at midnight UTC. Detects UTC day boundary in orchestrator tick loop. Reports: entries, exits, fees, net P&L, capital utilisation %, drawdown from peak. Sent via Telegram (`NotifyEvent::DailySummary`). Persisted to `daily_reports` SQLite table with full JSON data payload.
- `daily_reports` table in StateDB schema with `save_daily_report()` method.
- `parse_entry_ts()` helper in orchestrator for ISO/float timestamp parsing.

### Fixed
- **Seamless position close transition** (C4.1): Client-side buffer ensures closed table updates before open table removes transitioning positions. Eliminates visual gap on resolve/exit. 150ms fallback timeout.

### Documentation
- **OPS_RUNBOOK.md** (C5): 12-section operations runbook — VPS, scripts, CLI, dashboard, logs, circuit breaker, kill switch, Telegram, POL gas, backups, config, monitoring checklist.
- **USER_GUIDE.md** (C6): 11-section user guide — overview, setup, operation, dashboard, modes, multi-instance, recovery, safety, go-live checklist, glossary.
- **PROGRESS_ROADMAP.md retired** (C7): Replaced 1,270-line monolith with brief note pointing to 7 replacement documents.

---

## [0.14.6] — 2026-03-18 — C2: Kill Switch

### Added
- **Kill switch** (C2): Two trigger paths — `kill.sh --emergency` (file-based) and dashboard KILL SWITCH button (HTTP POST `/api/kill-switch`). Actions: cancel all open CLOB orders, set mode to shadow, send Telegram notification. Idempotent.
- `cancel_all_orders()` on `Executor`: Cancels all non-terminal tracked orders + CLOB cancel-all API call (L2 auth stub until Milestone D).
- Dashboard KILL SWITCH button in header with confirmation dialog.
- `kill.sh --emergency` flag writes `data/kill_switch.flag`, waits 5s for orchestrator, then graceful shutdown.

---

## [0.14.5] — 2026-03-18 — C1.1: POL Gas Monitor + State Safety

### Added
- **POL gas balance monitor** (`gas_monitor.rs`): Queries Polygon RPC `eth_getBalance` every hour. Wallet address auto-derived from private key. Warning < 1.0 POL → Telegram alert. Critical < 0.1 POL → trips circuit breaker. 3 unit tests.
- **State safety guards**: `.db.bak` backup created before every `load_from_disk()`. Save guard refuses to wipe positions if DB has some but runtime has 0.
- `GasCritical` circuit breaker trip reason for external gas-related trips.
- POL balance shown in stats log line (`POL=X.XXXX`).
- Config section `safety.gas_monitor` with 5 params.
- Automated POL bridge top-up added to Milestone G roadmap.

---

## [0.14.4] — 2026-03-18 — C1: Circuit Breaker

### Added
- **Circuit breaker module** (`circuit_breaker.rs`): Auto-pauses trading on 3 conditions — portfolio drawdown from peak, error burst in sliding window, API unreachable timeout.
- Peak total value persisted to SQLite; tripped state clears on restart (resume = restart).
- Housekeeping (state save, WS, reconciliation, stats) continues when tripped.
- Telegram notification on circuit breaker trip.
- 13 unit tests covering all trip conditions, expiry, disabled state, persistence.
- Dashboard `engine_status` shows `CIRCUIT_BREAKER` when tripped.
- Config section `safety.circuit_breaker` with 5 params: `enabled`, `max_drawdown_pct`, `max_consecutive_errors`, `error_window_seconds`, `api_timeout_seconds`.

### Fixed
- Circuit breaker API timeout 60s → 600s — original value was shorter than the 300s API resolution interval, causing false trips between periodic tasks.
- Startup `scanner.scan()` now records API success on the circuit breaker for extra safety margin.

### Changed
- Dashboard footer: brighter text color, added version number and copyright.

---

## [0.14.3] — 2026-03-18 — Telegram Notifications + Hostname Prefix

### Added
- **Telegram backend for notifications**: Auto-detected when webhook URL contains `api.telegram.org`. Sends `chat_id` + `text` JSON body. Bot token loaded from `secrets.yaml` (`telegram_bot_token`), chat ID from `config.yaml` (`phone_number`).
- **Hostname + instance prefix**: All notification messages prepended with `[hostname]` or `[hostname/instance]`. Hostname auto-detected from `/etc/hostname`, instance from `notifications.instance_name` config or `PT_INSTANCE` env var.
- **Startup notification**: `[STARTUP]` message sent on engine start with mode, open position count, and capital. Always sent (not toggleable).

### Changed
- Notifier module renamed from "WhatsApp" to generic "Notification" — supports Telegram, WhatsApp, ntfy.sh, Discord, etc.
- Dashboard stats bar: "Drawdown" label changed to "Drawdown (cur/max)" for clarity.
- Notifications enabled by default in config.yaml with chat_id `688371419`.

---

## [0.14.2] — 2026-03-18 — Milestone B Complete + Tier B 16 Connections

### Added
- **B5.0: ARCHITECTURE.md** — Post-Rust-port architecture: data flow diagram, component descriptions, file structure, full config reference, WS tier design, EIP-712 signing design decision, instrument model rounding table, glossary.
- **B5.1: ROADMAP.md** — All milestones (A–G) with goal, status, and task completion counts. Cross-references ARCHITECTURE.md.

### Changed
- **Milestone B marked COMPLETE** — all 27/27 tasks done (B1–B5). PRODUCT_SPEC_v2.md updated throughout.
- Tier B `max_connections` increased to 16 (jitter v2 + biased heartbeat provides sufficient spread at this level).
- PRODUCT_SPEC_v2.md: B4.0 documents shadow mode skip behaviour. B4.4 documents full chart/stats bar spec. WS connection budget updated for 16-conn default.

---

## [0.14.1] — 2026-03-18 — Shadow Reconciliation Fix + Dashboard P&L Charts

### Fixed
- **Reconciliation false alarms in shadow mode**: `reconcile_startup()` and `reconcile_periodic()` now detect empty venue credentials and skip CLOB comparison, preventing CRITICAL-level spam for all positions on VPS.

### Added
- **Unrealized P&L time-series**: Mark-to-market P&L computed from BookMirror live bids, tracked in `MonitorState` ring buffer, emitted via SSE full snapshots and deltas.
- **Portfolio chart P&L lines**: Unrealized P&L (yellow filled area) and Realized P&L (magenta dashed) alongside existing Total Value, Deployed %, Drawdown %.
- Chart legend ordered: dollar series first (Total Value, Unrealized P&L, Realized P&L), then percentage series (Deployed %, Drawdown %).

### Changed
- Portfolio chart: full width, 350px tall, linear Y-axis (was logarithmic — P&L can be negative).
- Financial stats bar reordered: Total Value → Deployed % → Win % → Drawdown (now/max) → Profit Factor → Recovery → Sharpe → Sortino → Avg Hold (smart units).
- Drawdown card now shows current/max combined (e.g. "1.23% / 4.56%").

---

## [0.14.0] — 2026-03-18 — B4 Reconciliation + Live P&L

### Added
- **B4.0: Periodic reconciliation** (`reconciliation.rs`): New module compares internal positions against CLOB API venue state every 5 min. Reports discrepancies by severity (Info/Warning/Critical). Detects: quantity mismatch, missing positions (both directions), orphan orders.
- **B4.1: Startup reconciliation**: Runs after state load, before entering event loop. Same comparison logic as periodic, with separate logging path.
- **B4.2: Cross-asset fill matching** (`detect_neg_risk_synthetics()`): Detects synthetic NO positions created by negRisk YES fills. Reports as informational discrepancies (expected behavior). Based on NT PR #3345/#3357 pattern.
- **B4.4: Live P&L tracking**: Dashboard now shows unrealized P&L per position (mark-to-market using BookMirror best bids) and total unrealized P&L in stats. Updated via SSE every 5s.
- 5 reconciliation tests: all_match, quantity_mismatch, missing_on_venue, missing_internal, neg_risk_synthetic, empty.
- `TradingEngine::reconcile_startup()` and `reconcile_periodic()` wrapper methods.

### Changed
- CLOB API query is stubbed (returns empty) until API credentials are configured — reconciliation runs but finds no venue-side data in shadow mode.

---

## [0.13.3] — 2026-03-18 — NT Review Hardening + WS Jitter v2

### Added
- **B4.3: Overfill detection** (`executor.rs`): `update_trade_status()` now clamps fill quantity to order quantity and tracks excess in `overfill_quantity` field. Based on NT issue #3221 pattern (clamp-and-track, position gets truth).
- **State transition validation** (`executor.rs`): `TradeStatus::can_transition_to()` rejects impossible state transitions (e.g., terminal→non-terminal, backwards flow). Based on NT issue #3403.
- 5 new executor tests: valid transitions, invalid transitions, overfill clamp.

### Changed
- WS reconnect jitter v2: healthy connections (lived > 30s) now add 0–3s extra random spread on reconnect, preventing cascade failures during Polymarket mass-disconnect events
- Batch order submission: now checks per-order `success` field and extracts `errorMsg` from Polymarket batch response (NT lesson). Enforces 15-order batch size cap.
- `update_trade_status()` returns `bool` (true if applied, false if rejected/unknown)
- Tier B `max_connections` increased to 12 (jitter v2 provides sufficient spread)

---

## [0.13.2] — 2026-03-17 — B2.4, B3.6, B3.7 + WS Reconnect Jitter

### Added
- **B2.4: Dynamic tick size handling** — `tick_size_change` WS events now update `InstrumentStore` in real-time. `InstrumentStore` threaded through TieredWsManager → ConnectionPool → handle_message_shared. Instrument precision (price/size/amount decimals) recalculated on tick change.
- **B3.6: Partial fill evaluation** (`executor.rs`): `evaluate_partial_fills()` + `evaluate_arb_fills()` — checks fill status of all arb legs, computes profit ratio, returns Accept/AcceptPartial/Unwind/NoFill decision. 6 unit tests.
- **B3.7: Batch order submission** (`executor.rs`): `execute_arb_batch()` — signs all legs upfront, submits as single `/orders` batch request. Falls back to sequential on batch endpoint failure. Aborts all legs if any fail during signing/validation.

- **InstrumentStore wired into engine** — `TradingEngine` owns `Arc<InstrumentStore>`, loaded from scanner data on every `ingest_scan_result()`. Passed to TieredWsManager so `tick_size_change` events update instruments live.

### Changed
- WS reconnect backoff now includes random jitter (50% of backoff + idx × stagger_ms) to prevent reconnect stampede after Polymarket mass-disconnects
- Dashboard clipboard: `execCommand('copy')` fallback for non-HTTPS contexts (fixes copy on Linux tab via VPS IP)
- Tier B `max_connections` restored to 11 (jitter fix should prevent the connection drops seen at 11)

### Fixed
- B-Part 2 & 3 checkboxes in PRODUCT_SPEC_v2.md now correctly marked as complete

---

## [0.13.0] — 2026-03-17 — EIP-712 Signing, Instrument Model & Fast-Market Support

### Added
- **EIP-712 order signing** (`signing.rs`): Pure Rust implementation using `alloy` crates
  - OrderSigner with pre-computed domain separators for CTF Exchange and Neg Risk CTF Exchange
  - Full EIP-712 typed data hashing (domain separator + order struct hash)
  - ECDSA signing via alloy-signer-local (PrivateKeySigner), signature type 0 (EOA)
  - `build_order()` helper with correct maker/taker amount computation for BUY and SELL
  - Correct taker address routing (Address::ZERO for regular, Neg Risk Adapter for negRisk)
  - 6 unit tests covering signing, amounts, domain separation, negRisk routing
- **Instrument model** (`instrument.rs`): Formal typed struct for Polymarket conditional tokens
  - `RoundingConfig` encoding py-clob-client's 4-tier ROUNDING_CONFIG (tick_size → price/size/amount precision)
  - `Instrument` struct: token_id, condition_id, neg_risk, tick_size, rounding, min/max order size, order_book_enabled
  - `InstrumentStore` (thread-safe RwLock): load from scanner data, update on tick_size_change events
  - 4 unit tests covering rounding, price validation, store load/update
- **Crypto price prediction category** (`types.rs`): `crypto_price` classification for short-lived 5-15 min markets (BTC/ETH/SOL above/below patterns)
- **`crypto_price` delay table entry**: 0.05h p95 (3 min) — crypto price markets resolve near-instantly on expiry
- **WS `tick_size_change` event handler**: Recognized and logged (previously fell through to unknown)
- **Scanner `tick_size` capture**: Now reads `minimum_tick_size` from Gamma API responses into market metadata
- **Shadow-F instance** in PRODUCT_SPEC_v2.md: Dedicated fast-market shadow for 5-15 min crypto price predictions
  - 60s constraint rebuild, 60s min resolution, 10s replacement cooldown, 0.1h replacement protection
  - 5% capital, 50 max positions, $200 max size, 1% min profit threshold
- **`order_aggression` config parameter**: `passive`/`at_market`/`aggressive` for tick offset control per instance

### Changed
- Shadow grid expanded from 5 to 6 instances (A–F) throughout PRODUCT_SPEC_v2.md
- Shadow-A `min_resolution_time_secs` lowered from 300 → 120 for overlap with fast-market testing
- WS connection budget updated: 11 Tier B + 6 Tier C = 17 total (within per-IP limit)
- Tier B `max_connections` tested at 11 — caused connection drops, reverted to 10

### Dependencies
- Added: `alloy-primitives`, `alloy-sol-types`, `alloy-signer`, `alloy-signer-local`, `hex`, `rand`

---

## [0.13.1] — 2026-03-17 — LiveExecutor, Rate Limiter & Trade Status Pipeline (B-Part 3)

### Added
- **LiveExecutor** (`executor.rs`): Full order execution pipeline for Polymarket CLOB
  - `OrderType` enum (FAK/GTC/FOK) and `OrderAggression` (passive/at_market/aggressive) for tick offset control
  - `compute_order_quantity()` — B3.0 quantity guard: FAK/GTC/FOK BUY/SELL → base (token count); market BUY → quote (USDC notional)
  - `Executor` struct with `execute_arb()` (two-leg), `execute_single_leg()`, `apply_aggression()`, `submit_to_clob()`
  - `TrackedOrder` struct with full lifecycle state (order_id, status, timestamps, fill amounts)
  - Trade tracking: `update_trade_status()`, `pending_orders()`, `timed_out_orders()`, `cleanup_old_orders()`
  - 11 unit tests (4 quantity guard, 6 timestamp parsing, 1 trade status lifecycle)
- **Trade status pipeline** (`executor.rs` B3.2): `TradeStatus` enum — Submitted → Matched → Mined → Confirmed / Retrying / Failed / Cancelled with `is_terminal()` predicate
- **Timestamp normalization** (`executor.rs` B3.4): `parse_polymarket_timestamp()` handles ISO8601 ±TZ, Unix seconds, Unix milliseconds, null/empty
- **Execution error handling** (`executor.rs` B3.5): `ExecutionError` enum covering rate-limited, insufficient balance, order rejected, network, timeout, signing, and unknown errors
- **Token-bucket rate limiter** (`rate_limiter.rs` B3.3): Multi-tier rate limiting for CLOB API
  - 4 simultaneous buckets: trading (60/min), public (100/min), auth (300/min), global (3000/10min)
  - `check(category) → Result<(), wait_secs>` with automatic token refund on global limit hit
  - `wait_and_consume()` blocking variant for simple usage
  - 3 unit tests (basic allow, category exhaust, global exhaust)

### Changed
- `lib.rs`: Added module declarations for `executor`, `rate_limiter`
- `config.yaml`: Tier B `max_connections` reverted from 11 → 10 (11 caused massive connection drops)

---

## [0.12.2] — 2026-03-17 — Dashboard Monitor Tab + Log Viewer

### Added
- **Monitor tab**: real-time system/app/financial metrics with time-series charts
  - System resources: CPU %, memory (MB), disk (GB) with live charts
  - App stats: markets, constraints, WS msg/s, latency p50/p95, queue depth
  - Financial stats: total value, deployed %, drawdown, Sharpe, Sortino, recovery ratio, win rate, profit factor, avg hold, max DD
  - Portfolio chart: combined dual-axis chart ($ value left, % deployed/drawdown right) with filled area for value
  - Profitability tables: by-category and by-duration breakdowns
  - Latency breakdown table: per-segment p50/p95/max/samples
- **Log viewer**: live tracing log capture in dashboard
  - `MonitorLayer` tracing subscriber pipes events to separate `LogRing` buffer (avoids monitor lock contention)
  - Configurable last-N display, text + level filtering, keyword highlighting (entry/exit/error/warn)
  - Copy-to-clipboard with proper delta tracking (only new entries per SSE update)
- **Splash screen**: waits for monitor data before dismissing — no empty graphs on load
- **Loading placeholders** for all monitor sections while data arrives
- Log-scale y-axes with smart auto-base (next lower power of 10 below data minimum)
- Chart tooltips with 2 decimal places, always showing all datasets

### Changed
- Brighter UI: stat labels, chart axes, legend text, tab buttons all brightened from #555/#888 to #999/#aaa
- Chart legends use thin line indicators (boxHeight: 2) instead of boxes; portfolio chart uses mixed style (filled box for value, lines for deployed/drawdown)
- Disk chart y-axis labels rounded to 2 decimal places
- Docs reorganized: older specs moved to docs/, PRODUCT_SPEC_v2.md is now active spec

### Removed
- `rust_arb/` — deprecated PyO3 arb library (superseded by `rust_engine/src/arb.rs`), archived to `archive/rust_arb_deprecated/`
- Orphan build artifacts: `rust_engine/target/` (1.6GB), `rust_engine/Cargo.lock`, `rust_engine/pyproject.toml`
- Zone.Identifier files, stale root-level log file

---

## [0.12.0] — 2026-03-17 — Tiered WebSocket Architecture

### Added
- **Tiered WS system** replacing flat sharding — solves Polymarket's undocumented 500 subscription/connection limit and per-IP connection cap
  - **Tier A**: REST-only universe scanning (every ~10min via MarketScanner) — unchanged
  - **Tier B**: Hot constraint monitoring pool (5-10 long-lived connections, max 450 assets each, ~2,000-3,000 assets)
  - **Tier C**: Open positions + command connection (1 connection, receives `new_market`, `market_resolved`, `best_bid_ask` global events)
- `ws_pool.rs` — Core `ConnectionPool` with long-lived connections, dynamic subscribe/unsubscribe via WS messages (no reconnection needed), auto-reconnect with exponential backoff, dynamic connection scaling
- `ws_tier_b.rs` — Tier B manager with hot constraint tracking, hysteresis on removal (3 consecutive cold scans before unsubscribe), hourly consolidation, promote/demote between tiers
- `ws_tier_c.rs` — Tier C manager with position asset management, new market event buffering (2.5s burst collection), `parse_new_market_event()` and `parse_market_resolved_event()` parsers
- `ws_tiered.rs` — Facade coordinating Tier B and Tier C with unified API for orchestrator
- **Asset migration protocol**: Subscribe on destination FIRST (overlap > gap), then unsubscribe from source — zero-gap coverage on position entry/exit
- **Incremental constraint rebuild**: `update_tier_b()` sends only subscription deltas instead of tearing down and rebuilding connections — **fixes connection leak bug** (old code spawned new shard tasks every ~10min without stopping old ones)
- **New market detection pipeline**: Tier C buffers `new_market` events, groups by `event_id`, flushes bursts to orchestrator for Tier B subscription
- Config toggle: `websocket.use_tiered_ws: true` to enable (default: false, old flat sharding preserved as fallback)
- Per-tier stats in log output: B connections/assets/hot constraints, C connections/assets/position count
- `constraint_to_assets` map in `DetectionResult` for Tier B hot constraint management

### Changed
- `detect_constraints()` now returns `(Vec<String>, HashMap<String, Vec<String>>)` tuple (all_asset_ids + constraint→assets map)
- `engine.start()` skips WS spawn when asset_ids is empty (dashboard-only mode for tiered WS)
- `drain_resolved()` now drains from both old WsManager and tiered WS resolved events
- `engine.stop()` stops both old WS and tiered WS
- Stats logging shows tiered breakdown when tiered WS is active
- `ws::handle_message` renamed to `handle_message_shared` and made `pub` for reuse by connection pool

### Fixed
- **INC-011**: WebSocket connection leak on constraint rebuild — old sharding code at orchestrator line 562 spawned new tasks without stopping old ones every ~10min, accumulating disconnected shards

---

## [0.11.1] — 2026-03-16 — Code Review v2: Bugs, Security, Performance & Style

### Fixed
- **B1**: Replacement scoring uses actual `end_date_ts` instead of hardcoded hours — imminent expirations now correctly prioritised
- **B2**: `debug_assert!` on closed-position count replaced with runtime guard — logs error and continues instead of silent UB in release builds
- **B3**: Latency ring buffer changed from `Vec<f64>` to `VecDeque<f64>` — eliminates O(n) shift on every tick
- **B5**: Sort comparisons use `f64::total_cmp` instead of `partial_cmp().unwrap()` — handles NaN consistently (eval.rs, orchestrator.rs)
- **B6**: Fallback workspace path changed from hardcoded `/home/andydoc/prediction-trader` to `.` (current directory)

### Security
- **S1**: API keys wrapped in `secrecy::SecretString` (zeroize-on-drop) in resolution.rs and postponement.rs
- **S2**: Config `--set` bounds validation — 12 numeric keys range-checked before applying (e.g. `capital_per_trade_pct` must be in (0, 0.5])

### Performance
- **P1**: `held_ids_cache` — cached `(HashSet, HashSet)` of held condition/market IDs, invalidated on 5 mutation paths; avoids rebuilding per tick

### Style & Maintainability
- **ST1**: Stale Python-era comments updated in state.rs and dashboard.rs
- **ST2**: All 7 `.unwrap()` calls on SQLite `prepare()`/`query_map()` in state.rs replaced with graceful `match` + `tracing::warn`
- **ST3**: `std::sync::Mutex` → `parking_lot::Mutex` in notify.rs — consistent with codebase, no poisoning

### Dependencies
- `secrecy = "0.10"` added to rust_engine

---

## [0.11.0] — 2026-03-16 — Notifications, Multi-Instance, Python Retirement

### Added
- **C3**: Notification module (`notify.rs`) — Telegram + generic webhook, rate-limited (10s), exponential backoff (5 failures → 5min cooldown), per-event toggles, hostname/instance prefix
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
