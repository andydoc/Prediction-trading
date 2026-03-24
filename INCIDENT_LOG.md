# Incident Log

Operational incidents for the Prediction Market Arbitrage System. Most recent first.

---

### INC-015: Cross-Filesystem Git Blindspot — 172 Lines Uncommitted (2026-03-24)

**Severity**: HIGH
**Impact**: `rust_engine/src/lib.rs` had 172 lines of uncommitted changes (module declarations for `usdc_monitor`, `fill_confirmation`, `sports_ws`, plus reconciliation return type fixes) that were invisible to Windows git but visible to WSL git. The E3 commit deployed to VPS failed to build because the reconciliation functions returned tuples but lib.rs declared single-value returns. VPS was unable to build for ~30 minutes during the E4 launch window.
**Root cause**: The repo lives on WSL's ext4 filesystem, mounted into Windows via `\\wsl.localhost\Ubuntu`. Windows git (`git diff HEAD`) showed no changes because it resolves paths differently than WSL git. The file was modified in WSL sessions but only WSL's `git diff` detected the delta. All prior commits were made from Windows git, silently skipping the WSL-only changes.
**Detection**: VPS `cargo build --release` failed with `E0308: mismatched types` on `reconcile_startup` and `reconcile_periodic`. md5sum comparison between local and VPS confirmed different file content despite same git commit hash.
**Fix**: Committed the missing changes from WSL git and pushed. VPS pull + rebuild succeeded.
**Prevention**: Always run `git status` and `git diff` from WSL (not Windows) when working on a WSL-hosted repo. Consider adding a pre-push hook that runs `git diff HEAD` from within WSL to catch cross-filesystem blindspots.
**Lessons**: Never trust Windows git to see changes on a WSL ext4 mount. The `P:\` drive mount is for editor convenience only — all git operations should go through WSL.

---

### INC-014: Strategy Gate Double-Division — Zero Virtual Positions (2026-03-22)

**Severity**: HIGH
**Impact**: All 6 virtual strategy portfolios (Shadow A-F) showed zero positions for the entire runtime. Strategy comparison data from the 350% shadow trading run was empty — all trades were main engine only, no virtual portfolio tracking occurred.
**Root cause**: `strategy_tracker.rs:passes_gates()` divided `expected_profit_pct` by 100, treating it as a percentage. But `arb.rs` already stores it as a decimal ratio (0.02 = 2%). The double division produced 0.0002, which is below every strategy's `min_profit_threshold` (0.01-0.05). Every opportunity was silently rejected.
**Detection**: Dashboard screenshot showed main engine with 1 open position but all 6 strategies at 0/0.
**Fix**: Removed `/100.0` division in `passes_gates()`. Profit threshold now compared directly against the decimal ratio.
**E2.6 impact**: Stress test restarted on VPS — prior run's strategy load was unrealistically low (no virtual positions = less eval queue pressure). Results would have underestimated worst-case load.
**Lessons**: Unit mismatches between modules need explicit documentation. The `expected_profit_pct` field name suggests percentage but stores decimal — consider renaming to `expected_profit_ratio` in future cleanup.

---

### INC-013: WS User Channel Auth — HMAC Sent Instead of Raw Secret (2026-03-20)

**Severity**: MEDIUM
**Status**: ✅ Resolved

**Summary**: WS User Channel subscription was sending an HMAC-SHA256 signature in the `secret` field instead of the raw base64url API secret. This caused authentication failures on the WebSocket user channel, preventing real-time fill tracking.

**Root Cause**: The `ws_user.rs` subscription builder called the same HMAC signing path used for REST endpoints (`ClobAuth::build_headers()`). Polymarket's WS User Channel expects raw API credentials (`apiKey`, `secret`, `passphrase`) with no HMAC computation — unlike REST endpoints which require HMAC-SHA256 signatures. This was not documented in Polymarket's public API docs; confirmed by reading the official `rs-clob-client` Rust SDK source.

**Impact**: D5 (multi-leg arb fill tracking) and D8 (closeout) were blocked. Fill tracker fell back to REST polling, masking the issue until the REST fallback was intentionally removed.

**Fix**:
1. Added `raw_secret_b64()` and `passphrase()` accessor methods to `ClobAuth` in `signing.rs`
2. Updated `ws_user.rs` to send raw credentials in the subscription `auth` field instead of HMAC-signed headers
3. Removed REST fallback from `fill_tracker.rs` to ensure WS path is exercised

**Lessons Learned**:
- REST and WS authentication models are fundamentally different on Polymarket CLOB: REST = HMAC-SHA256, WS = raw credentials
- Always verify auth against the official SDK source (`rs-clob-client`) when docs are ambiguous
- Removing fallback paths early forces bugs to surface rather than hiding behind workarounds

**Prevention**: Auth model documented in ARCHITECTURE.md ("CLOB Authentication: Dual Model" section).

---

### INC-012: VPS Migration — Germany to Madrid (2026-03-19)

**Severity**: MEDIUM
**Status**: ✅ Resolved (interim)

**Summary**: Migrated VPS from ZAP-Hosting Germany (193.23.127.99) to is*hosting Madrid (176.97.72.199) because Germany is geoblocked from Polymarket CLOB API.

**Root Cause**: Polymarket blocks 33 countries from CLOB API access, including Germany, UK, Netherlands. All CLOB order submissions returned 403 Forbidden from the Germany VPS.

**Resolution**:
- Contracted is*hosting for a Madrid VPS (Spain is not geoblocked)
- Dublin (Interxion DC) was first choice (0.83ms latency to CLOB) but had no capacity
- Host company offered free migration to Dublin when capacity becomes available
- IP address: 176.97.72.199 (same for both Madrid and eventual Dublin)

**Lessons Learned**:
- Always verify CLOB API geoblocking before provisioning VPS: `curl -I https://clob.polymarket.com`
- Polymarket geoblocked countries list: https://docs.polymarket.com/developers/CLOB/geoblock
- Allowed countries include: Ireland, Spain, Czech Republic (Prague)

**Follow-up**: Migrate to Dublin when capacity available (lower latency, free migration).

---

## INC-011: WebSocket Connection Leak on Constraint Rebuild

**Date**: 2026-03-17
**Severity**: High
**Markets**: All — affects entire WS subscription infrastructure
**Impact**: 11,000+ WS disconnects/day observed on VPS. Progressively degrading book freshness and increasing missed resolution events over time.

### What happened

The system was experiencing massive WebSocket instability on the VPS — thousands of disconnects per day, stale order books, and missed resolution events. Investigation revealed two root causes:

1. **Polymarket undocumented limits**: 500 subscriptions per connection (above this, initial snapshots stop arriving) and ~20-30 concurrent connections per IP.
2. **Connection leak in constraint rebuild**: Every ~10 minutes, `orchestrator.rs` line 562 called `self.engine.start(all_assets, 0, ...)` which spawned **new** shard tasks without stopping the old ones. Over hours, this accumulated dozens of zombie connections all competing for the same subscriptions.

### Root cause

The flat sharding approach (`assets_per_shard = 400`, creating ~6-8 shards) was fundamentally incompatible with Polymarket's connection limits. Combined with the leak, the system would exceed the per-IP connection limit within 2-3 hours of running.

### Fix

Implemented a three-tier WebSocket architecture:
- **Tier A**: REST-only scanning (no WS connections) — every ~10min
- **Tier B**: Hot constraint monitoring — 5-10 long-lived connections with dynamic subscribe/unsubscribe (no reconnection needed), hysteresis on removal (3 cold scans), hourly consolidation
- **Tier C**: Open positions + command connection — 1 connection, receives global events (`new_market`, `market_resolved`, `best_bid_ask`)

Constraint rebuilds now call `update_tier_b()` which sends incremental subscription changes instead of tearing down and rebuilding connections. Asset migration between tiers uses overlap-first protocol (subscribe destination before unsubscribing source).

Activation: Set `websocket.use_tiered_ws: true` in config.yaml.

**Status**: ✅ Implemented, pending production validation

---

## INC-010: Rust Initial Capital Defaulting to $1000

**Date**: 2026-03-16
**Severity**: Low
**Markets**: None directly
**Impact**: Dashboard showed incorrect capital ($1000 instead of $100). No trades affected — capital tracking is display-only until a position is opened.

### What happened

After the Rust-only cutover, the dashboard reported $1000 available capital. The Rust orchestrator had a hardcoded fallback of `1000.0` for `initial_capital` when the config key was missing or unparseable, rather than reading from `config.yaml` (which specifies `100`).

### Root cause

Hardcoded fallback value in `orchestrator.rs` didn't match the actual configured capital. The Python version read from config correctly.

### Fix

Changed `OrchestratorConfig` to read `initial_capital` from `config.yaml`, falling back to `100.0` (matching the actual bankroll).

**Status**: ✅ Resolved

---

## INC-009: No Periodic API Resolution Polling

**Date**: 2026-03-16 (identified during Cúcuta investigation)
**Severity**: Medium
**Markets**: All open positions relying on API resolution fallback
**Impact**: 2 positions stuck as "overdue" (Cúcuta Deportivo +$0.31, Sparta Praha +$0.55) for days after markets resolved on-chain.

### What happened

The Rust engine's `check_api_resolutions()` only ran at startup. If a WebSocket resolution event was missed (connection drop, message parsing error, etc.), there was no fallback — positions remained open indefinitely until the next full restart.

### Root cause

Python had periodic re-checks via its async event loop. The Rust port implemented the API resolution function but only called it once during initialisation, with no periodic schedule.

### Fix

Added periodic API resolution polling to the orchestrator tick loop, running every 5 minutes (configurable via `api_resolution_interval_seconds` in config). Immediately resolved the 2 overdue positions on first run after deployment.

**Status**: ✅ Resolved

---

## INC-008: outcomePrices Parsing Bug — API Resolution Non-Functional

**Date**: 2026-03-16 (identified during Cúcuta investigation)
**Severity**: High
**Markets**: All resolved markets since Rust port
**Impact**: API resolution path completely broken — no positions could ever resolve via API polling. Combined with INC-009 (no periodic polling), this meant resolution depended entirely on WebSocket events.

### What happened

Polymarket's CLOB API returns `outcomePrices` as a JSON string wrapping an array of strings: `"[\"0.123\", \"0.877\"]"`. The Rust implementation parsed this with `serde_json::from_str::<Vec<f64>>()`, which silently returned an empty Vec (strings aren't floats). Every market appeared to have zero-price outcomes, so the resolution logic never triggered.

Additionally, the resolution function inferred resolution status from prices (checking if any price was near 1.0) rather than checking the definitive `umaResolutionStatus` field from the UMA oracle. This meant even with correct price parsing, a market could appear "resolved" based on price movement before actual on-chain settlement.

### Root cause

Two compounding failures:
1. **Parsing**: `outcomePrices` is a string-encoded array of string-encoded floats — double serialisation not handled
2. **Resolution inference**: Prices near 0/1 don't guarantee resolution. The UMA oracle `umaResolutionStatus` field is the authoritative source.

### Fix

1. Parse `outcomePrices` as `Vec<serde_json::Value>`, then convert each element (handling both numeric and string representations)
2. Check `umaResolutionStatus == "resolved"` before reading prices — skip markets that haven't been settled by the oracle
3. Added periodic polling (see INC-009) so the fix actually runs regularly

**Status**: ✅ Resolved

---

## INC-007: State Lost on A1 Restart — Pre-existing State-Load Bug

**Date**: 2026-03-14
**Severity**: Medium
**Markets**: All open positions at time of restart
**Impact**: 4 open positions ($40.01 invested, $59.99 remaining capital) lost. Capital reset to $100.00.

### What happened

State loading was gated on `execution_state.json` existing, but actual state lives in `execution_state.db` (SQLite). When the system restarted for A1, `load_state()` was never called because the `.json` file didn't exist. The first save cycle then overwrote the SQLite DB with empty state.

### Root cause

Pre-existing bug since SQLite migration (v0.03.00). The JSON gate was never triggered because the system hadn't restarted since migration.

### Fix

Three commits on main:
1. Remove `.json` existence gate — always call `load_state()` (SQLite internally handles missing DB)
2. Move PM init after `rust_ws.start()` (was unreachable at step 1b, before rust_ws creation at step 6a)
3. Fix `open_count()` → `pm_open_count()` (PyO3 method name mismatch)

### Prevention

State backup before restart added. Pre-restart state snapshot recommended.

**Status**: ✅ Resolved

---

## INC-006: Argentine Fecha 9 Postponement — Capital Locked Until May

**Date**: 2026-03-11 (identified)
**Severity**: Low
**Markets**: 1360750–1360752 (River Plate vs CA Tucumán), 1360744–1360746 (Lanús vs CD Riestra)
**Impact**: $20.00 capital locked for ~55 extra days ($10 per position). No loss, but capital velocity degraded.

### What happened

Both matches were part of Argentine Liga Profesional Fecha 9, scheduled March 8, 2026. The AFA executive committee unanimously suspended all football activities March 5–8 in protest of an ARCA investigation into unpaid social security contributions. The entire round was rescheduled to the weekend of May 3, 2026. Positions entered before the postponement was announced.

### Root cause

External event (political/labour dispute) postponing an entire fixture round. The resolution validator cannot anticipate strike action.

### Fix

Positions correctly remain in `monitoring` — no phantom P&L. Capital returned when games are played (~May 3, 2026).

**v0.04.08**: `postponement_detector.py` now automatically detects overdue positions, searches the web for rescheduled dates, and updates resolution estimates. Replacement scoring uses AI-detected dates to evaluate whether to back out.

**Status**: ✅ Mitigated (positions held, AI postponement detection added)

---

## INC-005: WSL Disk Full — 200 GB Market Snapshots

**Date**: 2026-03-11 (identified and fixed)
**Severity**: High
**Impact**: WSL VHDX grew to 424 GB, leaving 5.21 GB free on C: drive. WSL became read-only.

### What happened

L1 market scanner (`market_data.py`) was writing timestamped 39 MB JSON snapshots every 30 seconds. Over 24 days: 20,383 files × 39 MB = 200 GB in `layer1_market_data/data/polymarket/`. Only `latest.json` is needed by the system.

### Root cause

`store_markets()` wrote both `latest.json` and `{timestamp}.json` on every collection cycle. No retention policy or disk monitoring existed.

### Fix

1. Removed timestamped snapshot writes (only `latest.json` kept)
2. Added `cleanup_old_logs(max_days=3)` to `main.py` startup
3. Manual cleanup: deleted 20,382 snapshot files, compacted VHDX via `diskpart compact vdisk`

**Status**: ✅ Resolved

---

## INC-004: TX-31 Republican Primary — Phantom Profit

**Date**: 2026-03-03 (identified), 2026-03-10 (cleared)
**Severity**: Low
**Markets**: 704392–704394 (Carter, Gomez, Hamden — 3 of 4+ outcomes; "Other" market missing)
**Impact**: $0.85 phantom profit. Cleared.

### What happened

Three positions opened Feb 21–24 (pre-validator era). Position [71] replaced by [380] normally; [380] and [465] expired by old `_expire_position()` on still-active markets. Rules state: resolves to "Other" if no nominee by Nov 3, 2026. No "Other" market exists on Polymarket.

### Root cause

Dual failure: time-based expiry on unresolved markets + unrepresented outcome not detected. Same class as INC-002.

**Status**: ✅ Resolved (cleared, guards in place since v0.03.00)

---

## INC-003: Arkansas Governor Democratic Primary — Phantom Profit

**Date**: 2026-03-03 (identified), 2026-03-10 (cleared)
**Severity**: Low
**Markets**: 824818–824819 (Love, Xayprasith-Mays — 2 of 3+ outcomes; "Other" and run-off markets missing)
**Impact**: $0.89 phantom profit. Cleared.

### What happened

Two positions opened Feb 22–27 (pre-validator) and expired by `_expire_position()` days before the actual primary (March 3, 2026).

### Root cause

Same class as INC-004 and INC-002: time-based expiry + incomplete outcome representation.

**Status**: ✅ Resolved (cleared, guards in place since v0.03.00)

---

## INC-002: Somaliland Parliamentary Election — Phantom Profit

**Date**: 2026-03-03 (identified and cleaned)
**Severity**: Medium
**Markets**: 948391–948394 (4 outcomes + "no election")
**Impact**: $0.93 phantom profit credited, then cleaned.

### What happened

System entered a mutex arb. API `endDate` was 2026-03-31 (scheduled election date), but rules text permitted resolution as late as 2027-03-31 (results unknown). After 28 hours, `_expire_position()` closed the position and credited expected profit as realised profit — on a market still actively trading.

### Root cause

Two compounding failures:
1. Time-based expiry credited phantom profit on unresolved markets
2. No validation that API `endDate` reflects the true latest resolution date in the rules

### Fix (v0.03.00)

1. Removed `_expire_position()`
2. Added `_check_group_resolved()` requiring price → 1.0 on all legs
3. Added `resolution_validator.py` (AI date validation)
4. Added `max_days_to_resolution: 60` filter

**Status**: ✅ Resolved

---

## INC-001: Japan Unemployment — Incomplete Mutex ($10 Loss)

**Date**: 2026-03-03 (identified and cleaned)
**Severity**: High
**Markets**: 1323418–1323422 (5 of 7 outcomes; outcomes for 2.6% and ≥ 2.7% missing)
**Impact**: $10.00 loss (all 5 positions lost). Only actual capital loss in system history.

### What happened

L1 had a hard cap of 10,000 markets. Polymarket had ~33,800. The two missing outcomes fell beyond the cutoff. L2 detected only 5 of 7 (sum = 0.889, passed the 0.85 guard). L3 direct path correctly blocked it (0.90 guard), but the Bregman/FW polytope fallback had no completeness guard — treated 5 incomplete markets as a valid arb. The outcome resolved to one of the two missing markets.

### Root cause

1. L1 market cap truncated available outcomes
2. L3 polytope path lacked mutex completeness guard

### Fix (v0.03.00)

1. L1 now paginates fully (33k+ markets)
2. L3 polytope path has mutex completeness guard at sum < 0.90

**Status**: ✅ Resolved

---

## Incident Template

```markdown
## INC-XXX: Title

**Date**: YYYY-MM-DD
**Severity**: Low / Medium / High / Critical
**Markets**: [market IDs if applicable]
**Impact**: [capital impact, operational impact]

### What happened

[Timeline and observable behaviour]

### Root cause

[Technical root cause]

### Fix

[What was done to resolve]

### Prevention

[What was done to prevent recurrence]

**Status**: 🔄 Investigating / ⚠️ Mitigated / ✅ Resolved
```
