# Architecture

Post-Rust-port architecture for the Prediction Market Arbitrage System. Current state only — not historical.

---

## Overview

Single compiled Rust binary (`prediction-trader`) with zero Python runtime. Two crates:

- **`rust_engine`** — core library: WebSocket, order book mirror, arb math, positions, dashboard
- **`rust_supervisor`** — binary entry point: CLI, config loading, orchestrator event loop

The supervisor creates a `TradingEngine` (the root of all shared state), starts the dashboard, loads markets, and enters the main event loop.

---

## Data Flow

```
Polymarket Gamma API (REST, ~10 min)
  │
  ▼
MarketScanner ──► Constraint Detector ──► ConstraintStore (DashMap)
  │                                            │
  ▼                                            ▼
InstrumentStore                          TieredWsManager
  (token→tick_size)                       ├─ Tier B: hot constraints (5-16 conns)
                                          └─ Tier C: positions + new markets (1 conn)
                                               │
                                               ▼
                                          BookMirror (DashMap)
                                            │ EFP drift detected
                                            ▼
                                          EvalQueue (condvar wake)
                                            │ drain urgent + background
                                            ▼
                                          Arb Math (mutex/polytope)
                                            │ rank by profit/time
                                            ▼
                                          PositionManager
                                            │ entry / resolution / replacement
                                            ▼
                                          Dashboard (Axum SSE)
```

### Startup Sequence

1. Parse CLI args, create PID file, setup signal handlers
2. Load `config/config.yaml` → create `TradingEngine`
3. Start dashboard (Axum HTTP on separate tokio task)
4. Fetch markets from Gamma API (with SQLite cache)
5. Detect constraints, build asset→constraint indices
6. Load state from SQLite (open positions, capital)
7. Start WS: Tier B (hot constraints) + Tier C (positions + new markets)
8. Startup reconciliation (compare internal state vs CLOB)
9. Enter main event loop (~250ms tick or condvar wake)

### Main Event Loop

Each iteration:
1. Drain eval queue (urgent first, then background)
2. For each constraint: read prices from BookMirror → arb math → opportunities
3. Filter by profit/resolution thresholds, score by `profit_pct / (hours + p95_delay)`
4. Enter top-N positions (capital check, leg calculation)
5. Check proactive exits (positions worth > 1.2× resolution payout)
6. Periodic tasks: state save, API resolution check, monitor refresh, constraint rebuild

---

## Component Descriptions

### BookMirror (`book.rs`)
Concurrent order book cache. `DashMap<asset_id, OrderBook>` with BTreeMap asks/bids. Applies snapshots and deltas from WS. Detects EFP (Expected Fair Price) drift when ask-side prices move > threshold, triggering urgent eval.

### EvalQueue (`queue.rs`)
Two-tier task queue (urgent + background) with condvar wakeup. Urgent: EFP drift from BookMirror. Background: stale refresh. Dedup via HashSet.

### Arb Math (`arb.rs`)
Pure Rust: `mutex_buy_all`, `mutex_sell_all`, polytope optimization (Frank-Wolfe). Computes optimal bet allocation across constraint legs.

### PositionManager (`position.rs`)
Full lifecycle: entry → monitoring → resolution/liquidation. Tracks capital accounting (initial, current, deployed). Replacement chains (B1.1): `chain_id`, `generation`, `parent_position_id`.

### TieredWsManager (`ws_tiered.rs`)
Facade coordinating Tier B + Tier C. Created lazily on first `start_tiered()` call.

### ConnectionPool (`ws_pool.rs`)
Long-lived WS connections with dynamic subscribe/unsubscribe (no reconnect needed). Auto-reconnect on error with exponential backoff. Biased heartbeat PING in `select!` to prevent timeout.

### Tier B (`ws_tier_b.rs`)
Hot constraint monitoring. 5-16 connections, ~2k-3k assets. Hysteresis: constraints stay subscribed for N scans after going cold. Hourly consolidation merges underutilised connections.

### Tier C (`ws_tier_c.rs`)
Single dedicated connection for open position assets + new market detection. Buffers new market events for 2.5s to collect full bursts.

### Dashboard (`dashboard.rs`)
Axum HTTP server: `GET /` (HTML), `/stream` (SSE), `/state` (JSON). HTML embedded via `include_str!`. SSE pushes full snapshots on connect, then deltas every 5s.

### MonitorState (`monitor.rs`)
TimeSeries ring buffers for system metrics (CPU, memory, disk, latency) and financial metrics (total value, deployed %, drawdown, realized P&L, unrealized P&L). Log ring buffer for dashboard log viewer.

### Constraint Detector (`detect.rs`)
Groups markets by `negRiskMarketID`. Builds mutual exclusivity, complementary, and logical implication constraints. Rebuilds every `constraint_rebuild_interval_seconds`.

### MarketScanner (`scanner.rs`)
Fetches Polymarket Gamma API, stores in SQLite cache (in-memory + disk backup via `CachedSqliteDB`).

### InstrumentStore (`instrument.rs`)
Maps `token_id` → `Instrument` (tick_size, rounding config, neg_risk flag, condition_id). Rounding rules match py-clob-client `ROUNDING_CONFIG`:

| tick_size | price_decimals | size_decimals | amount_decimals |
|-----------|---------------|---------------|-----------------|
| 0.1       | 1             | 2             | 3               |
| 0.01      | 2             | 2             | 4               |
| 0.001     | 3             | 2             | 5               |
| 0.0001    | 4             | 2             | 6               |

Updated dynamically on `tick_size_change` WS events via Tier C.

### Signing (`signing.rs`)
EIP-712 order signing for Polymarket CLOB Exchange. Pure Rust using `alloy-primitives` + `alloy-signer-local`. Signature type 0 (EOA). Supports both regular CTF Exchange and Neg Risk CTF Exchange contracts on Polygon (chain ID 137).

### Reconciliation (`reconciliation.rs`)
Compares internal position state against CLOB API venue state. Runs on startup and periodically (every 5 min). Detects: quantity mismatch, missing positions (both directions), orphan orders, negRisk synthetic fills. In shadow mode (no venue credentials): skips CLOB comparison, no false alarms.

### Executor (`executor.rs`)
Order execution: dry-run signing, live submission, trade status pipeline (MATCHED → MINED → CONFIRMED → FAILED), partial fill evaluation, batch order submission. Overfill detection clamps fill quantity and tracks excess.

### Rate Limiter (`rate_limiter.rs`)
Token bucket: 60 orders/min trading, 100 req/min public, 300 req/min auth, 3,000 req/10min global.

### Notifications (`notify.rs`)
Telegram (auto-detected) or generic webhook notifications. Bot token from `secrets.yaml`, chat_id from config. Rate limiting (10s), exponential backoff (5 failures → 5min cooldown). All messages prefixed with `[hostname/instance]`. Events: startup, entry, resolution, proactive exit, error, circuit breaker, daily summary.

---

## File Structure

```
rust_engine/src/
├── lib.rs              # TradingEngine: root of all shared state
├── types.rs            # Core types: OrderBook, OrderedFloat, EngineConfig
├── state.rs            # SQLite state persistence (positions, scalars)
├── cached_db.rs        # Generic in-memory SQLite with disk backup
├── book.rs             # BookMirror: concurrent order book cache
├── queue.rs            # EvalQueue: two-tier eval task queue
├── eval.rs             # Batch constraint evaluator + ConstraintStore
├── arb.rs              # Arb math: mutex, polytope (Frank-Wolfe)
├── position.rs         # PositionManager: lifecycle, capital accounting
├── detect.rs           # Constraint detector
├── scanner.rs          # Polymarket Gamma API market scanner
├── instrument.rs       # Instrument model (tick_size, rounding)
├── ws.rs               # Legacy flat WS manager (deprecated)
├── ws_pool.rs          # ConnectionPool: long-lived WS connections
├── ws_tiered.rs        # TieredWsManager: Tier B + C facade
├── ws_tier_b.rs        # Tier B: hot constraint monitoring
├── ws_tier_c.rs        # Tier C: position + new market monitoring
├── dashboard.rs        # Axum HTTP + SSE dashboard server
├── monitor.rs          # System/financial metrics + log ring
├── signing.rs          # EIP-712 order signing
├── executor.rs         # Order execution + trade status pipeline
├── reconciliation.rs   # Venue-side position reconciliation
├── resolution.rs       # Resolution validator (Anthropic API)
├── postponement.rs     # Postponement detector (Anthropic API)
├── rate_limiter.rs     # Token bucket rate limiter
├── latency.rs          # Latency percentile tracking
└── notify.rs           # Telegram / webhook notifications

rust_supervisor/src/
├── main.rs             # Binary entry: PID lock, signal handling
└── orchestrator.rs     # Event loop, market loading, position management

rust_engine/static/
└── dashboard.html      # Embedded via include_str! at compile time

config/
├── config.yaml         # Main configuration
└── instances/          # Per-instance overlays for multi-instance mode
```

---

## Config Reference

All parameterised values in `config/config.yaml`:

### Top-level
| Key | Default | Description |
|-----|---------|-------------|
| `mode` | `"dual"` | Operating mode: `shadow`, `live`, `dual` |

### `live_trading`
| Key | Default | Description |
|-----|---------|-------------|
| `enabled` | `true` | Allow live mode |
| `shadow_only` | `true` | Shadow orders only |
| `initial_capital` | — | Starting capital (USD) |
| `max_capital` | — | Dynamic limit per trade |
| `max_positions` | — | Open position cap |
| `min_orderbook_depth_usd` | — | Minimum liquidity |
| `depth_haircut` | `0.80` | Phantom order discount |
| `min_profit_ratio` | `0.70` | Pre-trade profit abort ratio |
| `trade_confirmation_timeout_seconds` | `120` | Max wait for CONFIRMED status |
| `order_aggression` | `at_market` | Tick offset: `passive`, `at_market`, `aggressive` |
| `reconciliation_interval_seconds` | `300` | Periodic reconciliation interval |

### `arbitrage`
| Key | Default | Description |
|-----|---------|-------------|
| `capital_per_trade_pct` | — | % of capital per trade |
| `min_trade_size` | — | Minimum trade (USD) |
| `max_position_size` | — | Maximum position (USD) |
| `max_exposure_per_market` | — | Max exposure per market (USD) |
| `max_concurrent_positions` | — | Position cap |
| `min_profit_threshold` | `0.03` | Minimum profit % to enter |
| `max_profit_threshold` | — | Maximum profit % (suspiciously high = skip) |
| `min_resolution_time_secs` | — | Minimum time to resolution |
| `max_days_to_resolution` | `60` | Maximum days to resolution |
| `replacement_cooldown_seconds` | `60` | Cooldown between replacements |
| `replacement_protection_hours` | `24` | Hours before position eligible for replacement |

### `engine`
| Key | Default | Description |
|-----|---------|-------------|
| `max_evals_per_batch` | `500` | Eval queue drain limit |
| `constraint_rebuild_interval_seconds` | `600` | Constraint re-detection interval |
| `efp_drift_threshold` | `0.005` | EFP drift % to trigger urgent eval |
| `state_save_interval_seconds` | `30` | SQLite state save interval |
| `monitor_interval_seconds` | `30` | System metrics collection interval |
| `stale_sweep_interval_seconds` | `60` | Stale asset cleanup interval |
| `api_resolution_interval_seconds` | `300` | API resolution poll interval |
| `closed_position_retention_days` | `90` | Days to keep closed positions |

### `websocket`
| Key | Default | Description |
|-----|---------|-------------|
| `use_tiered_ws` | `true` | Enable Tier B + C (replaces flat sharding) |
| `max_assets_per_connection` | `490` | Hard limit per WS connection (~500 Polymarket limit) |
| `stagger_ms` | `150` | Delay between connection startups |
| `tier_b_max_connections` | `17` | Max Tier B connections |
| `tier_b_hysteresis_scans` | `3` | Scans before unsubscribing cold constraint |
| `tier_b_consolidation_threshold` | `300` | Assets/conn below which hourly consolidation triggers |
| `tier_b_top_n_constraints` | `500` | Only subscribe top N constraints (0 = no limit) |
| `tier_c_new_market_buffer_secs` | `2.5` | New market event burst buffer |
| `heartbeat_interval` | — | PING interval (seconds) |
| `reconnect_base_delay` | — | Initial reconnect backoff |
| `reconnect_max_delay` | — | Max reconnect backoff |

### `optimization`
| Key | Default | Description |
|-----|---------|-------------|
| `algorithm` | `frank_wolfe` | Polytope solver algorithm |
| `max_iterations` | — | Convergence iteration limit |
| `tolerance` | — | Convergence threshold |

### `fees`
| Key | Default | Description |
|-----|---------|-------------|
| `trading_fee` | `0.0001` | Base trading fee (1 bp) |
| `polymarket_taker_fee` | — | Taker fee |

### `dashboard`
| Key | Default | Description |
|-----|---------|-------------|
| `port` | `5558` | HTTP server port |
| `bind_addr` | `127.0.0.1` | Bind address |

### `state`
| Key | Default | Description |
|-----|---------|-------------|
| `db_path` | `data/state_rust.db` | SQLite database path |

### `ai`
| Key | Default | Description |
|-----|---------|-------------|
| `provider` | `anthropic` | AI provider |
| `models.resolution_validation` | — | Model for resolution checks |
| `models.postponement_detection` | — | Model for postponement detection |

---

## Key Design Decisions

1. **Pure Rust, no PyO3** — eliminates Python runtime dependency entirely
2. **DashMap for concurrent reads** — lock-free reads on BookMirror and ConstraintStore
3. **parking_lot::Mutex** — faster than std::sync::Mutex for PositionManager, MonitorState
4. **EIP-712 signature type 0 (EOA)** — simplest signing path, no smart contract wallet needed
5. **`include_str!` for dashboard HTML** — single binary deployment, no static file serving; requires rebuild for HTML changes
6. **In-memory SQLite with disk backup** — fast reads, periodic flush to disk for crash recovery
7. **Tiered WS over flat sharding** — dynamic subscribe/unsubscribe without reconnect; biased heartbeat prevents timeout
8. **Condvar-based eval wake** — sub-ms response to price changes vs polling

---

## Glossary

| Term | Definition |
|------|-----------|
| **Constraint** | A group of related markets whose prices are linked (e.g., mutual exclusivity: prices must sum to ~$1) |
| **EFP** | Expected Fair Price — VWAP walk at trade size through the order book |
| **Tier A** | REST polling (Gamma API) for market discovery (~10 min intervals) |
| **Tier B** | WebSocket pool for hot constraint price monitoring (5-16 connections) |
| **Tier C** | Single WebSocket for open position monitoring + new market detection |
| **negRisk** | Polymarket's neg risk markets — YES fills create implicit NO positions on complementary outcomes |
| **FAK** | Fill And Kill — limit order that fills what it can immediately, cancels the rest |
| **Shadow mode** | Paper trading — all logic runs but no real orders submitted to CLOB |
| **Replacement chain** | Sequence of positions where each replaces the previous when a better opportunity appears |
| **p95 delay** | 95th percentile resolution delay by market category (accounts for UMA disputes, manual resolution) |
