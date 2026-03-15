# Incident Log

Operational incidents for the Prediction Market Arbitrage System. Most recent first.

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
