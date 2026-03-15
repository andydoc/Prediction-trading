# Rust Code Review: Prediction Market Arbitrage System

**Scope**: `rust_engine` (lib, 14 modules) + `rust_supervisor` (binary, 2 modules) — ~4500 LOC Rust
**Review date**: 2026-03-15

---

## Executive Summary

The codebase is well-structured for a Python-to-Rust port with good use of concurrency primitives (DashMap, parking_lot, atomics). The hot-path architecture (WS → BookMirror → EvalQueue → arb math) is sound. However, the iterative port has left behind several issues: one **critical functional bug** (WS resolution field swap), **NaN-safety gaps** in the order book, **unbounded memory growth** in resolved events, significant **JSON round-trip overhead**, and substantial **code duplication** from the port process.

**Total findings**: 8 bugs, 5 security, 10 performance, 8 inefficiencies, 3 hardware, 7 style, 4 debug logging, **+8 dependency risks**

---

## 0. DEPENDENCY AUDIT

Resolved versions verified against `Cargo.lock` and cross-referenced with RustSec advisory database.

**`cargo-audit` result**: 270 crate dependencies scanned against 950 advisories — **0 vulnerabilities found** (exit code 0). All known CVEs are at patched versions. The items below reflect unmaintained/deprecated status and version-lag risks that cargo-audit does not flag.

### DEP1 — CRITICAL: `serde_yaml 0.9.34` is deprecated and unmaintained
**Cargo.lock**: `serde_yaml 0.9.34+deprecated` (the version string literally says "deprecated")
- Archived since March 2024. No further security patches.
- Underlying `yaml-rust` has RUSTSEC-2024-0320 (unmaintained).
- The community fork `serde_yml` was also flagged unsound (RUSTSEC-2025-0068).
- **Fix**: Replace with `serde_yaml_ng` (maintained fork by acatton) or migrate config to TOML. Used in both `rust_engine/Cargo.toml` and `rust_supervisor/Cargo.toml`.

### DEP2 — OK (patched): `crossbeam-channel 0.5.15`
- RUSTSEC-2025-0024 / CVE-2025-4574: double-free race in `Channel::drop` affects 0.5.12–0.5.14.
- **Your lockfile resolves to 0.5.15 — PATCHED.** No action required.

### DEP3 — MEDIUM: `rusqlite 0.33.0` — 5 major versions behind
- Current latest is 0.38. The `bundled` feature vendors SQLite C source — staying current is important for upstream SQLite CVE fixes.
- Known Rust-side advisories (RUSTSEC-2020-0014, RUSTSEC-2021-0128) are patched in 0.33, but the bundled SQLite version may lack upstream C-level security fixes from 2024-2026.
- **Fix**: Upgrade to `rusqlite 0.38` (requires checking for API breakage — rusqlite makes breaking changes on major bumps).

### DEP4 — OK (patched): `tokio 1.50.0`
- RUSTSEC-2025-0023 (broadcast channel unsoundness) patched in >= 1.44.2. You have 1.50.0.
- All older advisories (RUSTSEC-2023-0001, 0005, 2021-0124) also patched. **No action required.**

### DEP5 — OK (patched): `rustls 0.23.37`
- RUSTSEC-2024-0336 (infinite loop DoS) patched >= 0.23.5.
- RUSTSEC-2024-0399 (fragmented ClientHello panic) patched in later 0.23.x.
- You have 0.23.37. **No action required.**

### DEP6 — OK (patched): `tracing-subscriber 0.3.23`
- RUSTSEC-2025-0055 / CVE-2025-58160 (ANSI escape injection) patched >= 0.3.20. You have 0.3.23. **No action required.**

### DEP7 — NOT PRESENT: `h2` (HTTP/2 CONTINUATION flood)
- RUSTSEC-2024-0332 affects h2 crate. **Your dependency tree does not include h2** — reqwest with `rustls-tls` and `default-features = false` avoids it. **No action required.**

### DEP8 — OK: Other dependencies verified clean
| Crate | Lockfile Version | Status |
|-------|-----------------|--------|
| chrono | 0.4.44 | Clean (RUSTSEC-2020-0159 patched >= 0.4.20) |
| dashmap | 6.1.0 | Clean (RUSTSEC-2022-0002 patched >= 5.1.0) |
| futures-util | 0.3.32 | Clean (RUSTSEC-2020-0059/0062 patched >= 0.3.15) |
| tungstenite | 0.24.0 | Clean (RUSTSEC-2023-0065 patched >= 0.20.1) |
| parking_lot | 0.12 | No advisories |
| axum | 0.8 | No advisories |
| clap | 4 | No advisories |
| nix | 0.30 | Clean (RUSTSEC-2021-0119 patched >= 0.23.0) |

### DEP9 — INFO: `serde` supply chain note
- Since Aug 2023, `serde_derive` ships precompiled binaries. No RUSTSEC advisory, but a supply chain transparency concern for a financial system. No practical alternative exists. **Awareness only.**

### Recommended new dependencies (verified clean)
| Crate | Purpose | Advisories | Notes |
|-------|---------|------------|-------|
| `secrecy` | Zeroize API keys on drop | None | RustCrypto ecosystem, `forbid(unsafe_code)` |
| `thiserror` | Library error types | None | Standard practice, maintained by dtolnay |
| `anyhow` | Application error handling | None | Standard practice, maintained by dtolnay |
| `smallvec` | Hot-path allocation reduction | 5 historical (all patched in 1.15.x) | Or use `tinyvec` (no unsafe) as alternative |
| `serde_yaml_ng` | Replace deprecated serde_yaml | None | Maintained fork of serde_yaml |

### Summary: Required dependency actions
| Priority | Action | Crate |
|----------|--------|-------|
| **CRITICAL** | Replace with `serde_yaml_ng` | serde_yaml 0.9 |
| **MEDIUM** | Upgrade to 0.38 for bundled SQLite security | rusqlite 0.33 |
| **Low** | Consider adding for API key safety | secrecy |

---

## 1. BUGS (potential correctness issues)

### B5 — CRITICAL: WS resolution event fields are swapped
**File**: `rust_supervisor/src/orchestrator.rs:423-424`
```rust
let events: Vec<(String, String)> = resolved.iter()
    .map(|r| (r.asset_id.clone(), r.market_cid.clone()))  // ← SWAPPED
    .collect();
let closed = self.engine.resolve_by_ws_events(&events);
```
`resolve_by_ws_events` expects `(condition_id, asset_id)` (see `position.rs:544`) but receives `(asset_id, condition_id)`. **WS-driven position resolution is broken** — positions only resolve via the API polling fallback (`check_api_resolutions`). Fix: swap the tuple order.

### B1 — HIGH: OrderedFloat NaN can corrupt BTreeMap
**File**: `rust_engine/src/types.rs:33-34`
```rust
self.0.partial_cmp(&other.0).unwrap_or(std::cmp::Ordering::Equal)
```
If a NaN price enters via malformed WS data, it compares `Equal` to everything, violating BTreeMap's total ordering invariant. This can corrupt the tree (lost/phantom levels, infinite loops in iteration).
**Fix**: Use `self.0.total_cmp(&other.0)` (stable since Rust 1.62). Also update `Hash` to be consistent — `total_cmp` treats `+0.0 == -0.0` and provides a total order over NaN.

### B3 — HIGH: Unbounded memory growth in resolved events
**File**: `rust_engine/src/ws.rs:466-498`
Events are pushed into `resolved` (line 468) and only cleared if positions actually resolve (line 498). Events for markets with no open positions accumulate forever. Over hours/days this vector grows without bound.
**Fix**: Cap the vector size, or clear events older than a threshold regardless of resolution success.

### B4 — MEDIUM: Latent TOCTOU race in liquidate_position
**File**: `rust_engine/src/position.rs:461-472`
`calculate_liquidation_value` acquires+releases the inner lock, then `liquidate_position` re-acquires it. Between these two locks, another thread could modify the position. Currently mitigated by the outer `Mutex<PositionManager>` in `TradingEngine`, but this is a design smell — if anyone calls `PositionManager::liquidate_position` directly, the race is live.
**Fix**: Compute the liquidation value inside the same lock scope as the removal.

### B2 — MEDIUM: Redundant double-locking on PositionManager
**File**: `rust_engine/src/lib.rs:52` + `position.rs:103`
`TradingEngine` wraps PM in `Arc<parking_lot::Mutex<PositionManager>>`, and PM wraps its data in an internal `Mutex<PositionManagerInner>`. Every operation acquires two locks. This is wasteful and fragile (lock ordering bugs if anything changes).
**Fix**: Remove the inner Mutex — let the outer one provide all synchronisation. Make `PositionManagerInner` the direct struct.

### B6 — LOW: validate_opportunity recomputes held sets inside loop
**File**: `rust_supervisor/src/orchestrator.rs:854-855`
`validate_opportunity` calls `get_held_constraint_ids()` and `get_held_market_ids()` each time, acquiring the positions lock twice. The caller already computed these sets. This wastes time and the sets could change between calls.
**Fix**: Pass the pre-computed sets as parameters.

### B7 — LOW: Fragile closed-position sync assumes append-only ordering
**File**: `rust_supervisor/src/orchestrator.rs:679`
`closed_jsons[db_closed..]` assumes new closed positions are always appended at the end. Currently true (only `push()` is used), but fragile.
**Fix**: Use position_id-based diffing rather than index-based slicing.

### B8 — INFO: PRAGMA WAL on in-memory SQLite is a no-op
**Files**: `state.rs:30`, `resolution.rs:53`, `postponement.rs:80`, `scanner.rs:29`
WAL journaling only applies to file-backed databases. Harmless but misleading.
**Fix**: Remove the PRAGMA from in-memory DB initialisation.

---

## 2. SECURITY

### S4 — HIGH: Unauthenticated dashboard on 0.0.0.0
**File**: `rust_engine/src/dashboard.rs:72`
The dashboard binds to `0.0.0.0` with no authentication, rate limiting, or CORS restrictions. Anyone on the network can read position data, capital, strategies via `/state` and `/stream`.
**Fix**: Bind to `127.0.0.1` by default. Add optional basic auth or token middleware. Use tower-http CORS (already a dependency but unused on dashboard routes).

### S1 — MEDIUM: Implicit TLS configuration on WS
**File**: `rust_engine/src/ws.rs:202`
`connect_async(ws_url)` relies on implicit TLS config. The engine does install rustls at `lib.rs:121`, which provides certificate verification by default. However, for a financial system, TLS should be explicitly configured and auditable.
**Fix**: Use `connect_async_tls_with_config` with an explicit `rustls` `ClientConfig` that pins or validates certificates.

### S3 — MEDIUM: CLI overrides write arbitrary YAML to config.yaml
**File**: `rust_supervisor/src/main.rs:173-191`
`--set key.path=value` writes directly to the config file on disk. An attacker with CLI access could inject arbitrary config values.
**Fix**: Validate override keys against an allowlist, or apply overrides in-memory only (don't persist to disk).

### S2 — LOW: API keys stored as plain String (no zeroize)
**Files**: `resolution.rs:382`, `postponement.rs:507`
API keys are `Mutex<String>`. The memory won't be zeroed on drop.
**Fix**: Consider `secrecy::Secret<String>` for zeroize-on-drop.

### S5 — LOW: User-Agent reveals system identity
**File**: `rust_engine/src/lib.rs:443`
`.user_agent("Mozilla/5.0 (prediction-trader)")` identifies the system to Polymarket.
**Fix**: Use a generic User-Agent.

---

## 3. PERFORMANCE

### P9 — HIGH: 50ms fixed polling in event loop
**File**: `rust_supervisor/src/orchestrator.rs:408`
```rust
std::thread::sleep(Duration::from_millis(50));
```
This sets a 50ms latency floor. The eval queue may have urgent items from EFP drift that sit waiting for the sleep to expire.
**Fix**: Use a `crossbeam::channel` or `tokio::sync::Notify` to wake the loop immediately when the eval queue has items, with a 50ms timeout as fallback.

### P1 — HIGH: Pervasive JSON serialize/deserialize round-trips
**Files**: `dashboard.rs:162-228`, `orchestrator.rs:652-657`, `orchestrator.rs:790-809`, `orchestrator.rs:933-941`
Positions are serialized to `Vec<String>` via `get_open_positions_json()`, then immediately deserialized back to `serde_json::Value` for field access. This happens every 5 seconds in the dashboard and every 30 seconds in state save. Example: `get_position_bids_by_id` serializes ALL positions just to find one by ID.
**Fix**: Add typed query methods to `PositionManager` (e.g., `get_position_bids(id) -> HashMap<String, f64>`) that return data directly without JSON round-trips.

### P6 — MEDIUM: HTTP client recreated per API resolution check
**File**: `rust_engine/src/lib.rs:440-450`
A new `reqwest::blocking::Client` (with fresh connection pool) is built on every call.
**Fix**: Store a persistent client on `TradingEngine` or in the orchestrator.

### P2 — MEDIUM: DashMap contention on hot path
**File**: `rust_engine/src/book.rs:96-113`
`check_efp_drift` performs 4 DashMap operations per book update: read from `books` → read from `last_efp` → write to `last_efp` → read from `asset_to_constraints`. Under high message rates this causes shard contention.
**Fix**: Store `last_efp` as a field inside the `OrderBook` struct (already in the `books` DashMap), reducing to 2 DashMap ops.

### P3 — LOW: Vec allocation on every book update
**Files**: `book.rs:55,65,81` — all return `Vec<(String, bool)>`
Most updates have no EFP drift and return empty vecs. Each `vec![]` allocates.
**Fix**: Use `SmallVec<[(String, bool); 2]>` or return an iterator.

### P4 — LOW: Repeated SystemTime::now() syscalls
**Files**: `ws.rs:324`, `queue.rs:52`, `eval.rs:121`, `position.rs:13`, `book.rs:165`
Each independently calls `SystemTime::now()`. A single timestamp per message/tick passed through the call chain saves syscalls.
**Fix**: Accept a `now: f64` parameter in hot-path functions.

### P5 — LOW: String cloning during constraint detection
**File**: `rust_engine/src/detect.rs:141-155`
Asset IDs and market IDs are cloned as `String` when building index maps. With 35k+ markets, this is significant allocation.
**Fix**: Use `Arc<str>` or string interning for IDs that are stored in multiple maps.

### P7 — LOW: Dashboard SSE rebuilds full JSON every tick
**File**: `rust_engine/src/dashboard.rs:119-146`
`build_stats` and `build_positions` are called every 5s with full serialization. Fine for 20 positions, but scales poorly toward the 500 max_positions config limit.
**Fix**: Cache the last JSON output and only rebuild when position state changes (dirty flag).

### P8 — LOW: classify_category does linear keyword scan
**Files**: `dashboard.rs:321`, `orchestrator.rs:38`
Iterates through ~40 keyword strings on every call.
**Fix**: Use Aho-Corasick automaton or precompiled regex. Also dedup (see I1).

### P10 — INFO: BTreeMap for 5-20 level order books
**File**: `rust_engine/src/types.rs:15-16`
BTreeMap gives O(log n) ops. For books with typically 5-20 price levels, a sorted `Vec` with binary search would have better cache locality and fewer allocations.
**Consider**: Only if profiling shows this is a bottleneck.

---

## 4. INEFFICIENCIES / CODE DUPLICATION

### I1: Duplicate `classify_category` function
**Files**: `dashboard.rs:321-355` and `orchestrator.rs:38-84`
Identical logic in two places.
**Fix**: Move to a shared module (e.g., `types.rs` or a new `utils.rs`).

### I2: Quadruplicated SQLite cache pattern
**Files**: `state.rs`, `resolution.rs:49`, `postponement.rs:69`, `scanner.rs:18`
~50 lines of identical `new()` / `mirror_to_disk()` / `load_from_disk()` copy-pasted 4 times.
**Fix**: Extract a generic `CachedSqliteDB` with a trait for schema initialization.

### I3: Many single-lock-per-call one-liners on TradingEngine
**File**: `rust_engine/src/lib.rs:332-336`
Methods like `current_capital()`, `total_value()`, `open_count()`, `closed_count()` each acquire the positions lock independently. When called in sequence this is N lock/unlock cycles for what should be one.
**Fix**: Provide a `with_positions<R>(f: impl FnOnce(&PositionManager) -> R) -> R` method.

### I4: Config re-read from disk on every constraint rebuild
**File**: `rust_supervisor/src/orchestrator.rs:565-569`
`detect_constraints` re-reads and re-parses `config.yaml` from disk every 10 minutes.
**Fix**: Cache the parsed YAML at startup; only re-read if the file changes.

### I5: Duplicate leg-building logic in dashboard
**File**: `rust_engine/src/dashboard.rs` lines ~417-441 (open) and ~718-741 (closed)
Nearly identical code for computing shares, formatting price, building leg JSON.
**Fix**: Extract a `build_leg_json()` helper function.

### I6: Duplicate message counter
**Files**: `ws.rs:41` (`WsManager::total_msgs`) and `book.rs:16` (`BookMirror::msg_count`)
Both count WS messages independently.
**Fix**: Use one authoritative counter.

### I7: Duplicate variable declarations in build_positions
**File**: `rust_engine/src/dashboard.rs:400-401` and `407-408`
`strategy` and `method` are declared at line 400-401, then shadowed with identical assignments at 407-408. The first pair is dead code.
**Fix**: Remove lines 400-402.

### I8: Redundant held set recomputation in replacement logic
**File**: `rust_supervisor/src/orchestrator.rs:773-774`
`held_cids` and `held_mids` are recomputed in the replacement section despite being computed at lines 433-434.
**Fix**: Reuse the already-computed sets.

---

## 5. HARDWARE RESOURCE USAGE

### H1: tokio runtime hard-coded to 2 worker threads
**File**: `rust_engine/src/lib.rs:146-147`
```rust
.worker_threads(2)
```
On a VPS with 4+ cores, the WS shards and dashboard could benefit from more parallelism.
**Fix**: Make configurable, or default to `std::thread::available_parallelism()`.

### H2: Blocking HTTP calls on orchestrator thread
**Files**: `orchestrator.rs` (scanner.scan, check_api_resolutions, resolution_validator, postponement_detector)
The orchestrator's main loop uses `std::thread::sleep` on a regular OS thread, so blocking HTTP calls don't block the tokio runtime. However, they do block the eval loop — no price evaluation happens during API calls (scanner refresh, API resolution checks, AI validation). With multiple positions and retry logic, this could be several seconds of dead time.
**Fix**: Run blocking API work on a separate `std::thread` or `tokio::task::spawn_blocking`.

### H3: SQLite double-mutex overhead
All SQLite connections use `parking_lot::Mutex` externally but also SQLite's internal serialized-mode mutex.
**Fix**: Open connections with `SQLITE_OPEN_NOMUTEX` flag to disable SQLite's internal mutex, since all access is already serialized by parking_lot.

---

## 6. STYLE

### ST1: Stale Python-era comments throughout
Multiple doc comments still reference Python: `ws.rs:27,73`, `eval.rs:18,76`, `position.rs:501`, `queue.rs:5`, `dashboard.rs:34`, `book.rs:151`.
**Fix**: Update all comments to reflect the current pure-Rust architecture.

### ST2: Inconsistent error handling
Mix of `Result<_, String>`, `let _ =` error swallowing (e.g., `state.rs:66,84,106`), and bare `.unwrap()` (e.g., `state.rs:93`). No unified error type.
**Fix**: Create a crate-level `Error` enum (or use `anyhow`/`thiserror`). Replace `.unwrap()` on SQLite operations with proper error propagation.

### ST3: Missing `#[must_use]` on key return types
`EvalBatchResult`, `ApiResolution`, `ScanResult`, `DetectionResult`, `EntryResult` should be `#[must_use]`.

### ST4: `pub` visibility too broad
Many internal types and methods are `pub` that should be `pub(crate)` — artefact of the PyO3 era where everything needed to be public for Python FFI.

### ST5: Dead code — `BookMirror::len()` and `live_count()` are identical
**File**: `rust_engine/src/book.rs:138-144`
Two methods with the same body. Remove one.

### ST6: Magic numbers scattered throughout
- `100.0` synthetic depth in `apply_best_prices` (book.rs:86-87)
- `0.899` threshold in `check_mutex_arb` (arb.rs:44)
- `12` max markets in `polytope_arb` (arb.rs:141)
- `200` max latency samples (orchestrator.rs:446)
- `1.2` proactive exit multiplier hardcoded in multiple places

**Fix**: Move to named constants or config values.

### ST7: `ConstraintStore` and `EvalQueue` should impl `Default`
Both have `new()` that could be `Default::default()`.

---

## 7. EXTRANEOUS DEBUG LOGGING

### D1 — HIGH VOLUME: Every WS message logged at debug level
**File**: `rust_engine/src/ws.rs:301`
```rust
tracing::debug!("Shard {} raw msg: {}", shard_id, &text[..text.len().min(200)]);
```
At 100+ msgs/sec across 9 shards, this generates ~1000 log entries/sec. Even at `debug` level, the string formatting and truncation happen before the level check (tracing evaluates arguments eagerly in this macro form).
**Fix**: Remove or gate behind `tracing::trace!`.

### D2 — MEDIUM VOLUME: Unknown event type logged per message
**File**: `rust_engine/src/ws.rs:342`
Fires for every unrecognized event type. Could be noisy if Polymarket adds new event types.
**Fix**: Log at `trace` level or add a rate-limited counter.

### D3 — LOW: Cache hit logged on every postponement lookup
**File**: `rust_engine/src/postponement.rs:569`
**Fix**: Remove — cache hits are the expected happy path.

### D4 — LOW: Stale assets logged every 60s even when empty
**File**: `rust_supervisor/src/orchestrator.rs:496`
**Fix**: Only log when count > 0 (already conditional, but the debug fires even for non-actionable counts).

---

## Recommended Priority Order for Fixes

1. **B5** (field swap) — Critical functional bug, likely causing silent resolution failure
2. **B1** (NaN handling) — Data corruption risk on malformed WS data
3. **B3** (unbounded events) — Memory leak in long-running production
4. **D1** (WS debug logging) — Immediate log volume / disk space issue
5. **P9** (50ms polling) — Easy win for latency improvement
6. **B2** (double-lock removal) — Simplifies concurrency model
7. **P1** (JSON round-trips) — Systematic refactor with broad performance benefit
8. **I2** (SQLite cache dedup) — Code maintainability
9. **S4** (dashboard auth) — Required before any network exposure
10. Everything else
