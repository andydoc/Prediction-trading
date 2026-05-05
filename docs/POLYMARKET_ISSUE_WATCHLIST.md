# Polymarket Open-Issue Watchlist

Survey of currently-open issues across Polymarket's trading-relevant repos
(2026-05-05). Filtered to ones that could affect our V2 Rust arb bot. See
`INCIDENT_LOG.md` INC-021 for our V2 migration history.

Repos surveyed: `py-clob-client`, `py-clob-client-v2`, `clob-client` (TS),
`rs-clob-client`, `rs-clob-client-v2`, `clob-order-utils`,
`python-order-utils`, `go-order-utils`, `ctf-exchange`, `ctf-exchange-v2`,
`neg-risk-ctf-adapter`, `agents`, `poly-market-maker`,
`builder-relayer-client`, `builder-signing-sdk`.

Snapshot tag: **v0.19.0** (2026-05-05). When `api_drift_monitor` detects a new
py-clob-client release, a Telegram alert fires with the release notes and a
reminder to re-run the re-audit process (see bottom of this file).

## Critical: HIGH-relevance

| # | Repo | Issue | Description | Status | Code location |
|---|------|-------|-------------|--------|--------------|
| 1 | rs-clob-client-v2 | #36 `sum_of_matched_orders` accumulator | Server-side counter ratchets up post-V2 cutover and never decrements; pins free placeable balance near zero after sustained activity. | **MITIGATED** — parse from 400 response in `submit_to_clob`; returns `ExecutionError::BalancePinned`; orchestrator trips circuit breaker after 10+ occurrences in 30-trade window. | `executor.rs::submit_to_clob`, `circuit_breaker.rs::RevertRateTracker`, `orchestrator.rs::alert_on_api_contract_rejection` |
| 2 | rs-clob-client-v2 | #34 / #23 `MakerOrder.fee_rate_bps` empty string | V2 server emits `""` for protocol-controlled fees; strict `Decimal` deserialize fails the entire `trades()` page. | **N/A for our codebase** — we do not use a strongly-typed `MakerOrder` struct. `ws_user.rs::parse_trade_event` extracts fields manually with `parse_str`/`parse_f64` helpers, which handle empty strings gracefully. Verified 2026-05-05. | `ws_user.rs::parse_trade_event` |
| 3 | rs-clob-client-v2 | #30 `post_orders` returns `id: ""` | Empty-string id treated as valid downstream → stuck untrackable orders. | **MITIGATED** — `executor.rs::submit_to_clob` already returns `ExecutionError::ClobRejection { code: "EMPTY_ORDER_ID" }` on empty id (lines 873–878). We use single `/order` POSTs only, not `/orders`. | `executor.rs:873` |
| 4 | py-clob-client | #338 V2 settlement reverts ~35% on profitable orders | matchOrders V2 reverts with empty reason on profitable orders far more than losing ones. On-chain bug, not client. | **MITIGATED** — `RevertRateTracker` (30-trade window, min 10 fills, 35% threshold) trips `CircuitBreaker::check_revert_critical` via `circuit_breaker.rs::TripReason::RevertRate`. Currently records `Reverted` on `BalancePinned` events; deeper on-chain tracking deferred. | `circuit_breaker.rs::RevertRateTracker`, `orchestrator.rs::alert_on_api_contract_rejection` |
| 5 | py-clob-client-v2 | #41 / #38 Cloudflare 403 on `/auth/api-key` | Post-V2 the `/auth/api-key` path intermittently gets a Cloudflare challenge HTML response. Library doesn't surface a clean error. | **MITIGATED** — `signing::is_cloudflare_html()` detects HTML body; both `derive_api_key` and `create_api_key` return CF-specific errors. `submit_to_clob` returns `ExecutionError::CloudflareChallenge`; orchestrator alerts once/hour. | `signing.rs::is_cloudflare_html`, `executor.rs::submit_to_clob` |
| 6 | py-clob-client-v2 | #40 L2 credentials TTL ~30 min | Undocumented server-side expiry on L2 creds. Re-deriving returns the same UUID and re-activates. | **MITIGATED** — `submit_to_clob` returns `ExecutionError::Unauthorized` on 401. `alert_on_api_contract_rejection` sets `last_clob_auth_refresh = 0.0` to force immediate re-derive on next periodic tick (vs waiting 24h). | `executor.rs:Unauthorized`, `orchestrator.rs::alert_on_api_contract_rejection` |

## MEDIUM: wired into the runtime

| # | Repo | Issue | Description | Status | Code location |
|---|------|-------|-------------|--------|--------------|
| 7 | py-clob-client | #292 WSS silent freeze | Connection OPEN, ping/pong fine, zero `book` events for hours. | **MITIGATED** — `TieredWsManager::check_freeze` tracks total message count; if unchanged for 300s with active subscriptions, returns `true`. Orchestrator alerts via Telegram (rate-limited to once/10 min). | `ws_tiered.rs::check_freeze`, `orchestrator.rs` |
| 8 | rs-clob-client | #325 / #280 / rs-v2 #39 WS / FD leak | Arc cycle, ~120 sockets/min in long-running daemon. | **MITIGATED** — `impl Drop for TieredWsManager` calls `self.stop()` so threads are cleaned up on drop. | `ws_tiered.rs::Drop` |
| 9 | rs-clob-client | #225 Order book hash mismatch | Several wrappers can't reproduce server-side book hash. | **DEFERRED** — bug is upstream; do not add hash verification until Polymarket fixes it. |  |
| 10 | rs-clob-client | #226 `market_resolved` events not delivered on `/ws/market` | Only `new_market` arrives. | **MITIGATED** — rely on Gamma `closed`/`resolutionTime` polling (`check_api_resolutions`). |  |
| 11 | rs-clob-client | #330 `BookUpdate` bids descending / asks ascending — actual is reversed | Our orderbook ingestion: verify we sort explicitly by price rather than trusting incoming order. | **MITIGATED** — both `apply_snapshot` and `apply_delta` insert into `BTreeMap` which auto-sorts. `apply_delta` (types.rs:122) does not trust incoming order. Verified 2026-05-05. | `types.rs::apply_delta` |
| 12 | py-clob-client | #300 `get_balance_allowance` double-counts on fast follow-up | Server-side cache race. | **MITIGATED** — 30s debounce on CLOB balance reads; 2× jump triggers a re-read and takes the lower of the two values. | `usdc_monitor.rs::check_balance_with_auth` |
| 13 | py-clob-client | #323 market-order rounds with `round_normal` not `round_down` | SDK divergence; FAK semantics. | **MITIGATED** — `compute_order_quantity` uses `round_size_floor` (floor division) for `OrderType::Fak` BUY orders; GTC/FOK still use `round_size` (nearest). | `executor.rs::compute_order_quantity`, `instrument.rs::round_size_floor` |
| 14 | py-clob-client | #294 SDK swaps maker/taker on SELL → invalid signature | V1 reference bug. We're V2 + Rust. | **MITIGATED** — verified by D2 SELL probe (cleanup 2026-05-01 23:52 — accepted). |  |
| 15 | clob-client (TS) | #327 SDK leaks auth headers in error logs | TS-side bug; check we don't have the same issue. | **MITIGATED** — `submit_to_clob` NetworkFailure path uses `e.without_url()` (no URL, no headers in error string). Auth header values (`POLY_API_KEY`, `POLY_SIGNATURE`, etc.) are never included in reqwest Error.to_string(). | `executor.rs::submit_to_clob` |
| 16 | py-clob-client | #273 `Request exception!` + 5s timeout but order matches | Retry on timeout duplicates orders. | **MITIGATED** — `submit_to_clob` maps timeout → `ExecutionError::Timeout` with no auto-retry; caller (orchestrator) does not retry on Timeout. | `executor.rs::submit_to_clob` |
| 17 | neg-risk-ctf-adapter | #39 EOA redeem `redeemPositions` returns no-op | Open question, no Polymarket answer. We need redeem on resolved negRisk markets eventually. | **DEFERRED** — write manual redeem test on a resolved negRisk market before depending on this path. |  |

## LOW: monitored, not active concern for our setup

| # | Repo | Issue | Why low-risk for us |
|---|------|-------|---------------------|
| 18 | rs-clob-client | #319 `cancel_order` sends `orderId` not `orderID` | We use `DELETE /cancel-all`, not per-order cancel (`executor.rs::cancel_all_orders` calls `/cancel-all` directly per V2 docs we verified). |
| 19 | rs-clob-client | #272 `derive_proxy_wallet` PROXY_INIT_CODE_HASH outdated | Doesn't apply — we're EOA, not proxy. |
| 20 | rs-clob-client | #239 / #229 SDK heartbeats / orderbook subscription broken | We don't use SDK WS path. |
| 21 | rs-clob-client | #294 `BuilderTradeResponse.builder` Address vs UUID | We don't query `/builder/trades`. |
| 22 | builder-relayer-client | #27 / #11 / #15 Gasless / redeem flaky / Gnosis-Safe-only | We don't use builder-relayer; sign locally + submit our own tx. |
| 23 | python-order-utils | #51 `generate_seed()` uses `random.random` (non-crypto) | **MITIGATED** — `random_salt()` changed to `OsRng` (signing.rs:524). |
| 24 | poly-market-maker | #87 / #80 their MM bot bugs | Not our codebase. |
| 25 | clob-client | #311 createOrDeriveApiKey returns invalid creds for ~2 min | Onboarding-only path; we already do this manually one-time. |

## Already mitigated by prior INCs

- **Ghost fills** (V2 post-cutover, MATCHED with no chain settlement) — see py-clob-client-v2 #34. Our `executor.rs::process_fill_event` only promotes `Submitted → Matched → Confirmed`, with `enter_suspense` on Matched and `confirm_from_suspense` on Confirmed. Phase A bug 3 (phantom-payout reversal in `check_api_resolutions`) is the second line of defence.
- **Gamma keyset cursor ignored** (#334, closed) — affected our 2026-04-29 keyset migration; reverted in commit `f505d67`. Filter still broken (this is what `POLYMARKET_GAMMA_NEGRISK_FILTER_ISSUE.md` is about — separate issue).
- **`balance: 0` despite valid pUSD allowances** (V2 spender = NegRiskAdapter) — handled in our wrap script with all 6 V2 allowances (commit `78e5caa`).

## Audit queue — COMPLETED 2026-05-05

All items below were actionable code changes. Completed in this session:

| Audit item | Resolution |
|-----------|------------|
| Auth-header scrubbing in error logs (#15) | `e.without_url()` in NetworkFailure path — headers were never in reqwest errors anyway. ✅ |
| Salt RNG: `OsRng` not `SmallRng` (#23) | `random_salt()` now uses `rand::rngs::OsRng`. ✅ |
| `submit_to_clob` retry policy: no transport-level retry (#16) | Verified correct; documented as MITIGATED. ✅ |
| Market-order rounding: `round_down` for FAK BUY (#13) | `round_size_floor()` added to `RoundingConfig`; used in `compute_order_quantity` for `Fak+Buy`. ✅ |
| Orderbook ingestion: explicit price sort, no trust in `BookUpdate` order (#11) | `BTreeMap` auto-sorts in both `apply_snapshot` and `apply_delta`. Verified MITIGATED. ✅ |
| Balance double-read debounce (#12) | 30s debounce + 2× jump re-read in `usdc_monitor.rs`. ✅ |
| Lenient deserializer on `MakerOrder` / `Trade` for empty-string fields (#2) | N/A: we use manual JSON field extraction, not strict serde structs. ✅ |
| Revert-rate kill switch (#4) | `RevertRateTracker` + `TripReason::RevertRate` + `check_revert_critical`. ✅ |
| `sum_of_matched_orders` monitor (#1) | Parse from 400 body; `ExecutionError::BalancePinned`; circuit-breaker integration. ✅ |

## Process

### When a new Polymarket release ships

The `api_drift_monitor` polls `Polymarket/py-clob-client` GitHub releases hourly.
On new tag detection, a **Telegram alert fires automatically** with:
- Tag, published date, and link
- First 800 chars of the release notes
- Reminder to re-audit this file

**Manual re-audit steps** (takes ~30 min):

1. Read the GitHub release notes carefully. Focus on:
   - Order payload field changes (`salt`, `side`, `metadata`, `builder`, `expiration`)
   - Auth endpoint changes (`/auth/api-key`, `/auth/derive-api-key`)
   - Wire-format changes in response objects (`trades`, `orders`, `fills`)
   - New or removed query parameters

2. For each open issue in the HIGH/MEDIUM tables above, re-check if it was closed or changed:
   ```
   https://github.com/Polymarket/py-clob-client/issues?q=is%3Aissue+is%3Aclosed
   https://github.com/Polymarket/rs-clob-client/issues?q=is%3Aissue+is%3Aclosed
   ```

3. Update the table rows: change status, add/update code location, note the release.

4. Run the test suite on VPS:
   ```bash
   ssh madrid-ubuntu 'cd /home/ubuntu/prediction-trader && cargo test --release 2>&1 | tail -20'
   ```

5. Update the snapshot tag at the top of this file to the new release tag.

6. Commit the updated watchlist:
   ```bash
   git add docs/POLYMARKET_ISSUE_WATCHLIST.md
   git commit -m "watchlist: re-audit against py-clob-client <NEW_TAG>"
   ```

### Delta report format (expected in Telegram alert)

```
[API-DRIFT release] py-clob-client v1.X.Y (was v1.X.Z). Published 2026-XX-XX.
https://github.com/Polymarket/py-clob-client/releases/tag/v1.X.Y

Release notes:
<first 800 chars of GitHub release body>

⚠️ Re-audit docs/POLYMARKET_ISSUE_WATCHLIST.md against this release.
Check for order-payload / auth / field changes. Update watchlist snapshot.
```
