# Rust Code Review v2: Prediction Market Arbitrage System

**Scope**: `rust_engine` (lib, 15 modules) + `rust_supervisor` (binary, 2 modules) — ~5200 LOC Rust
**Review date**: 2026-03-16
**Codebase version**: v0.11.0 (post-review fixes from v0.10.1 applied)

---

## Executive Summary

The codebase has improved markedly since the v0.10.1 review. All critical and high-severity bugs from the first review have been fixed: NaN safety uses `total_cmp()`, the WS resolution field swap is corrected, the double-lock on PositionManager is removed, unbounded WS events are cleared, and the 50ms polling is replaced with Condvar-based waking. Dependencies are current (serde_yaml_ng, rusqlite 0.38). A generic `CachedSqliteDB` eliminates the quadruplicated SQLite pattern. The dashboard binds to 127.0.0.1 and the user-agent is generic.

What remains are **medium-severity performance items** (per-tick HashSet rebuilds, Vec reallocation for latencies), a few **correctness edge cases** (replacement scoring hardcodes 24h, debug_assert in release), **minor security items** (plain-String API keys, unvalidated config override values), and **style cleanup** (stale Python comments, unwrap on SQLite ops).

**Total findings**: 7 bugs/edge-cases, 3 security, 5 performance, 3 style, 1 dependency note

---

## 0. DEPENDENCY AUDIT

**`cargo-audit` result**: 271 crate dependencies scanned against 950 RustSec advisories — **0 vulnerabilities found** (exit code 0).

| Crate | Lockfile Version | Status |
|-------|-----------------|--------|
| serde_yaml_ng | 0.9.36 | Clean — maintained fork, replaces deprecated serde_yaml |
| rusqlite | 0.38.0 | Clean — current major version |
| tokio | 1.50.0 | Clean — all advisories patched |
| rustls | 0.23.37 | Clean — all advisories patched |
| crossbeam-channel | 0.5.15 | Clean — CVE-2025-4574 patched |
| tungstenite | 0.24.0 | Clean |
| reqwest | 0.12.28 | Clean |
| h2 | Not present | reqwest with rustls-tls avoids it |

**No action required on dependencies.**

---

## 1. BUGS / CORRECTNESS EDGE CASES

### B1 — MEDIUM: Replacement scoring hardcodes 24-hour horizon
**File**: `rust_supervisor/src/orchestrator.rs:896`
```rust
let hours_rem = 24.0_f64; // simplified
```
Replacement decisions compare new-opportunity score vs worst-held-position score. The held position's remaining value is divided by a fixed 24h estimate instead of actual hours to resolution. This makes all replacement scoring equally rough regardless of whether a position resolves in 2 hours or 30 days.
**Fix**: Use the position's actual `end_date_ts` from metadata:
```rust
let end_date_ts = pos.metadata.get("end_date_ts").and_then(|v| v.as_f64()).unwrap_or(0.0);
let hours_rem = ((end_date_ts - now_secs()) / 3600.0).max(0.01);
```

### B2 — MEDIUM: `debug_assert!` provides no protection in release builds
**File**: `rust_supervisor/src/orchestrator.rs:751`
```rust
debug_assert!(n_closed_total >= db_closed, "closed positions shrunk: {} < {}", n_closed_total, db_closed);
```
In release builds, if `n_closed_total < db_closed` (e.g., after a state corruption or position data mismatch), `closed_rows_data[db_closed..]` could panic on out-of-bounds or silently skip data.
**Fix**: Replace with a runtime guard:
```rust
if n_closed_total < db_closed {
    tracing::error!("Closed position count decreased: {} < {} (skipping incremental sync)", n_closed_total, db_closed);
    return;  // or do a full resync
}
```

### B3 — MEDIUM: Latency Vec reallocates on every full cycle
**File**: `rust_supervisor/src/orchestrator.rs:488-490`
```rust
self.recent_latencies = self.recent_latencies[self.recent_latencies.len()-MAX_LATENCY_SAMPLES..].to_vec();
```
Once the latency buffer fills (200 entries), every subsequent tick with evaluations allocates a new Vec and copies 200 f64s.
**Fix**: Use `VecDeque<f64>` with `push_back` / `pop_front`:
```rust
self.recent_latencies.push_back(batch_us);
while self.recent_latencies.len() > MAX_LATENCY_SAMPLES {
    self.recent_latencies.pop_front();
}
```

### B4 — LOW: Proactive exits re-lock positions for per-exit bid lookup
**File**: `rust_supervisor/src/orchestrator.rs:1060-1066`
`collect_all_position_bids()` acquires the positions lock, computes all bids, releases. Then for each exit candidate, `get_position_bids_by_id()` re-acquires the lock. Between these calls, positions could theoretically change. Also wastes lock cycles.
**Fix**: Collect per-position bids alongside all_bids in the first lock acquisition.

### B5 — LOW: Eval sort uses `partial_cmp` with `Equal` fallback for NaN
**File**: `rust_engine/src/eval.rs:263`
```rust
opportunities.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
```
Same pattern at `orchestrator.rs:83`. If `score` is NaN (unlikely but possible if arb math produces NaN profit_pct), NaN opportunities sort as Equal and could be selected.
**Fix**: Use `b.score.total_cmp(&a.score)` (stable since Rust 1.62).

### B6 — LOW: Hardcoded fallback path in main.rs
**File**: `rust_supervisor/src/main.rs:92`
```rust
.unwrap_or_else(|| PathBuf::from("/home/andydoc/prediction-trader"));
```
Developer-specific Unix path. Won't work on other machines or Windows. Currently mitigated by `--workspace` and `TRADER_WORKSPACE` env var, but the fallback is misleading.
**Fix**: Use `.` (current directory) as fallback, or require the workspace argument.

### B7 — INFO: PID lock TOCTOU race
**File**: `rust_supervisor/src/main.rs:406-437`
The PID check reads the file, checks if the PID is alive, then removes and recreates. Between check and recreate, another process could start. Low risk in practice (single-user system).
**Fix**: Use `O_EXCL` atomic file creation pattern.

---

## 2. SECURITY

### S1 — MEDIUM: API keys stored as plain String (no zeroize)
**Files**: `resolution.rs:381`, `postponement.rs:507`, `orchestrator.rs:221`
API keys in `Mutex<String>`. Memory won't be zeroed on drop. If process crashes or dumps core, keys are readable.
**Fix**: Consider `secrecy::Secret<String>` for zeroize-on-drop. Low effort, high signal for a financial system.

### S2 — LOW: Config override values not range-checked
**File**: `rust_supervisor/src/main.rs:231-254`
The `ALLOWED_SET_KEYS` allowlist prevents arbitrary key injection (good!). But values for allowed keys aren't validated. `--set "arbitrage.capital_per_trade_pct=999"` or `"engine.state_save_interval_seconds=0.0001"` would be accepted.
**Fix**: Add bounds validation for numeric keys after `parse_yaml_value()`.

### S3 — LOW: WS TLS relies on implicit rustls defaults
**File**: `rust_engine/src/ws.rs:202`
```rust
let (ws_stream, _response) = tokio_tungstenite::connect_async(ws_url).await?;
```
Rustls IS installed as the default crypto provider (`lib.rs:125`), so TLS verification does happen. However, for a financial system, explicit TLS configuration with auditable certificate handling would be stronger.
**Fix**: Use `connect_async_tls_with_config` with an explicit rustls `ClientConfig`.

---

## 3. PERFORMANCE

### P1 — MEDIUM: `get_held_ids()` rebuilds HashSets every tick
**File**: `rust_supervisor/src/orchestrator.rs:476`
```rust
let (held_cids, held_mids) = self.engine.get_held_ids();
```
Called every ~50ms. Iterates all open positions, allocates two HashSets. With 20 positions, that's ~80 String insertions per tick (1600/sec). The sets only change on position entry/exit/resolution.
**Fix**: Cache the held sets on the Orchestrator struct. Invalidate (set dirty flag) when `enter_position`, `liquidate_position`, `close_on_resolution`, or `resolve_by_ws_events` is called. Only recompute when dirty.

### P2 — LOW: `ConstraintStore::get()` clones the full Constraint
**File**: `rust_engine/src/eval.rs:56-58`
```rust
pub fn get(&self, id: &str) -> Option<Constraint> {
    self.constraints.get(id).map(|r| r.clone())
}
```
`Constraint` contains `Vec<MarketRef>` with multiple Strings. Every `evaluate_batch` call clones one per constraint evaluated. DashMap's `Ref` guard could be used instead.
**Fix**: Return `dashmap::mapref::one::Ref<'_, String, Constraint>` or use `with()` closure pattern.

### P3 — LOW: `log_stats` clones latency Vec for sorting
**File**: `rust_supervisor/src/orchestrator.rs:1174`
```rust
let mut lats = self.recent_latencies.clone();
```
200 f64s cloned every 30s. Trivial cost but easily avoidable with a pre-sorted ring buffer or by sorting in-place on a temporary copy.

### P4 — LOW: Repeated `SystemTime::now()` syscalls
**Files**: Multiple (eval.rs:133, book.rs:140,167, position.rs:13, ws.rs:324)
Each independently calls `SystemTime::now()`. A single timestamp per tick passed through the call chain would save syscalls on the hot path.

### P5 — INFO: DashMap contention in `check_efp_drift`
**File**: `rust_engine/src/book.rs:90-108`
`check_efp_drift` stores `last_efp` inside the `OrderBook` (already in the `books` DashMap entry), then looks up `asset_to_constraints` (separate DashMap). This is 2 DashMap operations vs the 4 from the previous review — improved. Further optimization would require architectural changes (embedding constraint IDs in the OrderBook entry).

---

## 4. STYLE / QUALITY

### ST1 — Stale Python-era comments
- `state.rs:4-5`: `eliminating ~30-40ms of GIL contention every 30s` — references Python GIL
- `dashboard.rs:44`: `Metrics updated by the Python engine loop` — should say Rust orchestrator
**Fix**: Update to reflect pure-Rust architecture.

### ST2 — `.unwrap()` on SQLite prepare/query_map in state.rs
**File**: `rust_engine/src/state.rs:85,86,127,138,153,165`
Multiple `.unwrap()` calls. If the DB is corrupted or schema changes, these panic.
**Fix**: Use `?` operator or `.ok()` with graceful fallback.

### ST3 — Notifier uses `std::sync::Mutex` instead of `parking_lot::Mutex`
**File**: `rust_engine/src/notify.rs:87-89`
Inconsistent with rest of codebase. Requires `unwrap_or_else(|e| e.into_inner())` pattern for poisoning.
**Fix**: Switch to `parking_lot::Mutex` for consistency. No poisoning concern.

---

## 5. PREVIOUSLY FIXED (verified)

The following issues from the v0.10.1 review are **confirmed resolved**:

| ID | Issue | Status |
|----|-------|--------|
| B5 | WS resolution field swap | ✅ Correct: `(r.market_cid, r.asset_id)` matches `resolve_by_ws_events(&[(cid, aid)])` |
| B1 | OrderedFloat NaN | ✅ Uses `total_cmp()` (types.rs:35) |
| B2 | Double-lock on PositionManager | ✅ Removed — single outer `Arc<Mutex<PM>>` |
| B3 | Unbounded resolved events | ✅ Always cleared after processing (ws.rs:497) |
| B8 | WAL pragma on in-memory SQLite | ✅ Removed — CachedSqliteDB uses `PRAGMA synchronous=OFF` only |
| P9 | 50ms fixed polling | ✅ Condvar-based `wait_for_work()` (queue.rs:88) |
| P6 | HTTP client recreated per call | ✅ Stored on TradingEngine (lib.rs:61) |
| H1 | Hardcoded 2 worker threads | ✅ `available_parallelism()` (lib.rs:151) |
| S3 | CLI overrides: no allowlist | ✅ `ALLOWED_SET_KEYS` array (main.rs:231) |
| S4 | Dashboard on 0.0.0.0 | ✅ Binds to 127.0.0.1 (dashboard.rs:70) |
| S5 | User-Agent reveals system identity | ✅ Generic browser UA (lib.rs:159) |
| I1 | Duplicate classify_category | ✅ Single copy in types.rs:337 |
| I2 | Quadruplicated SQLite cache | ✅ Generic `CachedSqliteDB` (cached_db.rs) |
| I3 | Multiple lock acquisitions for held IDs | ✅ Combined `get_held_ids()` (lib.rs:418) |
| DEP1 | Deprecated serde_yaml | ✅ Migrated to serde_yaml_ng 0.9.36 |
| DEP3 | Outdated rusqlite 0.33 | ✅ Upgraded to 0.38.0 |
| D1 | Every WS message logged at debug | ✅ Now `tracing::trace!` |
| D2 | Unknown event type logged per message | ✅ Now `tracing::trace!` |
| ST7 | Missing Default impls | ✅ ConstraintStore + EvalQueue impl Default |
| #[must_use] | Key return types | ✅ Added to EvalBatchResult, ApiResolution, EntryResult, Opportunity |

---

## Recommended Priority Order for Remaining Fixes

1. **B1** (replacement scoring 24h) — Easy fix, direct correctness improvement for replacement decisions
2. **B2** (debug_assert in release) — Guard against panic in production
3. **P1** (held_ids caching) — Biggest remaining performance win
4. **B3** (VecDeque for latencies) — Simple refactor, eliminates per-tick allocation
5. **B5** (total_cmp for sort) — Consistency with the NaN fix already applied to OrderedFloat
6. **S1** (secrecy for API keys) — Add dependency, wrap key storage
7. **ST1-ST3** (style cleanup) — Low effort polish
8. Everything else

---

## Verification

- `cargo build --release` in workspace root — must compile clean
- `cargo test` — all existing tests pass (notify.rs has 4 unit tests)
- `cargo clippy -- -W clippy::all` — should be clean or improve
- `cargo audit` — already confirmed 0 vulnerabilities
- Manual: start with `--shadow-only`, verify dashboard on 127.0.0.1:5558, confirm WS connects and eval loop runs
