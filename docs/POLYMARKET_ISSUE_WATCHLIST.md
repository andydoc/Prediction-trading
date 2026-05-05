# Polymarket Open-Issue Watchlist

Survey of currently-open issues across Polymarket's trading-relevant repos
(2026-05-05). Filtered to ones that could affect our V2 Rust arb bot. See
`INCIDENT_LOG.md` INC-021 for our V2 migration history.

Repos surveyed: `py-clob-client`, `py-clob-client-v2`, `clob-client` (TS),
`rs-clob-client`, `rs-clob-client-v2`, `clob-order-utils`,
`python-order-utils`, `go-order-utils`, `ctf-exchange`, `ctf-exchange-v2`,
`neg-risk-ctf-adapter`, `agents`, `poly-market-maker`,
`builder-relayer-client`, `builder-signing-sdk`.

## Critical: HIGH-relevance UNMITIGATED

| # | Repo | Issue | Description | Action |
|---|------|-------|-------------|--------|
| 1 | rs-clob-client-v2 | #36 `sum_of_matched_orders` accumulator | Server-side counter ratchets up post-V2 cutover and never decrements; pins free placeable balance near zero after sustained activity. | **Add monitor**: parse `sum_of_matched_orders` from 400 responses; circuit-break if persistently pinned. |
| 2 | rs-clob-client-v2 | #34 / #23 `MakerOrder.fee_rate_bps` empty string | V2 server emits `""` for protocol-controlled fees; strict `Decimal` deserialize fails the entire `trades()` page. | **Audit**: add lenient deserializer (`#[serde(default, deserialize_with = "empty_as_zero")]`) wherever we parse trade / maker-order responses. |
| 3 | rs-clob-client-v2 | #30 `post_orders` returns `id: ""` | Empty-string id treated as valid downstream → stuck untrackable orders. | **Pre-validate** before relying on `executor::execute_arb_batch` (currently we use single `/order` POSTs only — guarded for now). |
| 4 | py-clob-client | #338 V2 settlement reverts ~35% on profitable orders | matchOrders V2 reverts with empty reason on profitable orders far more than losing ones. On-chain bug, not client. | **External; can't fix** but **add a revert-rate kill switch** (>X% reverts in rolling window pauses execution). |
| 5 | py-clob-client-v2 | #41 / #38 Cloudflare 403 on `/auth/api-key` | Post-V2 the `/auth/api-key` path intermittently gets a Cloudflare challenge HTML response. Library doesn't surface a clean error. | **Detect**: if response Content-Type is `text/html` or body starts with `<!DOCTYPE html>`, treat as transient + retry with backoff. |
| 6 | py-clob-client-v2 | #40 L2 credentials TTL ~30 min | Undocumented server-side expiry on L2 creds. Re-deriving returns the same UUID and re-activates. | **MITIGATED in commit `<TBD>`** — auto-rederive on 401 from L2 endpoints. See `signing.rs::ClobAuth` integration. |

## MEDIUM: worth wiring into the test harness

| # | Repo | Issue | Description | Action |
|---|------|-------|-------------|--------|
| 7 | py-clob-client | #292 WSS silent freeze | Connection OPEN, ping/pong fine, zero `book` events for hours. | **Watchdog**: per-token last-event timestamp; force resubscribe if no event > N seconds while top-of-book is moving on REST. |
| 8 | rs-clob-client | #325 / #280 / rs-v2 #39 WS / FD leak | Arc cycle, ~120 sockets/min in long-running daemon. | We use a custom Rust WS per `ws_tiered.rs`; **audit** Drop semantics on disconnect. |
| 9 | rs-clob-client | #225 Order book hash mismatch | Several wrappers can't reproduce server-side book hash. | Don't add hash verification yet — bug is upstream. |
| 10 | rs-clob-client | #226 `market_resolved` events not delivered on `/ws/market` | Only `new_market` arrives. | We rely on Gamma `closed`/`resolutionTime` polling for resolution detection (already implemented in `check_api_resolutions`). **MITIGATED.** |
| 11 | rs-clob-client | #330 `BookUpdate` bids descending / asks ascending — actual is reversed | Our orderbook ingestion: **verify** we sort explicitly by price rather than trusting incoming order. |
| 12 | py-clob-client | #300 `get_balance_allowance` double-counts on fast follow-up | Server-side cache race. | **Debounce** balance polls + one re-read on suspicious 2× jump. |
| 13 | py-clob-client | #323 market-order rounds with `round_normal` not `round_down` | SDK divergence; FAK semantics. | We use FAK; **confirm** our market-order path rounds DOWN for buys to match TS canonical. Audit `compute_order_quantity` rounding. |
| 14 | py-clob-client | #294 SDK swaps maker/taker on SELL → invalid signature | V1 reference bug. We're V2 + Rust. | Verified by D2 SELL probe (cleanup at 2026-05-01 23:52 — accepted). **MITIGATED.** |
| 15 | clob-client (TS) | #327 SDK leaks auth headers in error logs | TS-side bug; check we don't have the same issue. | **Audit** our reqwest error handler; ensure auth headers are scrubbed from error strings. |
| 16 | py-clob-client | #273 `Request exception!` + 5s timeout but order matches | Server may have accepted the order despite client timeout. Retry on timeout duplicates orders. | **Verify** our `submit_to_clob` does NOT auto-retry on transport-level timeout (only retries on explicit 5xx with no order id). |
| 17 | neg-risk-ctf-adapter | #39 EOA redeem `redeemPositions` returns no-op | Open question, no Polymarket answer. We need redeem on resolved negRisk markets eventually. | **Test pre-scale**: write a manual redeem test on a real resolved negRisk market before depending on it. |

## LOW: monitored, not active concern for our setup

| # | Repo | Issue | Why low-risk for us |
|---|------|-------|---------------------|
| 18 | rs-clob-client | #319 `cancel_order` sends `orderId` not `orderID` | We use `DELETE /cancel-all`, not per-order cancel (`executor.rs::cancel_all_orders` calls `/cancel-all` directly per V2 docs we verified). |
| 19 | rs-clob-client | #272 `derive_proxy_wallet` PROXY_INIT_CODE_HASH outdated | Doesn't apply — we're EOA, not proxy. |
| 20 | rs-clob-client | #239 / #229 SDK heartbeats / orderbook subscription broken | We don't use SDK WS path. |
| 21 | rs-clob-client | #294 `BuilderTradeResponse.builder` Address vs UUID | We don't query `/builder/trades`. |
| 22 | builder-relayer-client | #27 / #11 / #15 Gasless / redeem flaky / Gnosis-Safe-only | We don't use builder-relayer; sign locally + submit our own tx. |
| 23 | python-order-utils | #51 `generate_seed()` uses `random.random` (non-crypto) | Verify our salt generator uses `OsRng`, not `SmallRng`. (Audit item.) |
| 24 | poly-market-maker | #87 / #80 their MM bot bugs | Not our codebase. |
| 25 | clob-client | #311 createOrDeriveApiKey returns invalid creds for ~2 min | Onboarding-only path; we already do this manually one-time. |

## Already mitigated by prior INCs

- **Ghost fills** (V2 post-cutover, MATCHED with no chain settlement) — see py-clob-client-v2 #34. Our `executor.rs::process_fill_event` only promotes `Submitted → Matched → Confirmed`, with `enter_suspense` on Matched and `confirm_from_suspense` on Confirmed. Phase A bug 3 (phantom-payout reversal in `check_api_resolutions`) is the second line of defence.
- **Gamma keyset cursor ignored** (#334, closed) — affected our 2026-04-29 keyset migration; reverted in commit `f505d67`. Filter still broken (this is what `POLYMARKET_GAMMA_NEGRISK_FILTER_ISSUE.md` is about — separate issue).
- **`balance: 0` despite valid pUSD allowances** (V2 spender = NegRiskAdapter) — handled in our wrap script with all 6 V2 allowances (commit `78e5caa`).

## Audit queue (low effort, high value)

These don't have a corresponding upstream issue but are suggested by the
patterns in the survey. Walk our own code at next maintenance window.

- [ ] Auth-header scrubbing in error logs (per #15).
- [ ] Salt RNG: `OsRng` not `SmallRng` (per #23).
- [ ] `submit_to_clob` retry policy: no transport-level retry (per #16).
- [ ] Market-order rounding: `round_down` for buys (per #13).
- [ ] Orderbook ingestion: explicit price sort, no trust in `BookUpdate` order (per #11).
- [ ] Balance double-read debounce (per #12).
- [ ] Lenient deserializer on `MakerOrder` / `Trade` for empty-string fields (per #2).
- [ ] Revert-rate kill switch (per #4).
- [ ] `sum_of_matched_orders` monitor (per #1).

## Process

When a new Polymarket release ships (the `api_drift_monitor` already
polls py-clob-client releases per commit `6297507`), re-run this survey.
A delta against this snapshot tells us what shifted.
