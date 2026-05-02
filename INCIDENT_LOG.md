# Incident Log

Operational incidents for the Prediction Market Arbitrage System. Most recent first.

---

### INC-021: Polymarket CLOB Schema Mismatch — Live Halted, Six Bugs Surfaced (2026-04-29)

**Severity**: HIGH (live execution non-functional, phantom accounting accumulating)
**Status**: HALTED — `shadow_only: true` set on Dublin 2026-04-29 05:25 UTC; remediation pending

**Trigger**: Operator noticed the Strategies page and Positions page disagreed
about the state of a Guangzhou-temperature `mutex_sell_all` constraint —
Positions reported a "preemptive exit" had occurred, Telegram + Strategies
said the trade was still live, and there had been "no live trade" in the
wallet sense. Investigation surfaced six interlocking issues, summarised
below.

**Forensic snapshots** (preserved on Dublin):
- `data/state_rust.db.inc021_pre_halt_20260429_052507Z` — full DB at halt
- `config/config.yaml.inc021_pre_halt_20260429_052507Z` — pre-flip config
- `data/forensics/inc021_pre_halt_20260429_052507Z.txt` — text-mode dump of
  scalars / positions / strategy_portfolios / strategy_open_positions /
  evaluated_opportunities (last 5)

**Timeline (UTC 2026-04-29)**:

| Time | Event |
|---|---|
| 00:07:08 | Mutex_sell_all opportunity fires on Guangzhou temperature 25°C / ≥26°C, 13.6% expected. Live PM creates paper position `paper_1777421228...`. `executor.execute_arb` called. |
| 00:07:08 | **CLOB rejects both legs**: `HTTP 400 {"error":"order_version_mismatch"}`. No real shares acquired. |
| 00:07:23 → 03:38 | `check_proactive_exits` fires every ~30s for ~3.5h, ratio 1.34→2.46. INC-020 path correctly refuses to credit phantom proceeds (zero fills). Position holds. |
| 03:28 / 03:33 | UMA dispute detected on both legs (`uma_status=proposed`). Bot logs warning, does not act. |
| 03:38:56 | `check_api_resolutions` finds the market resolved (winner=2092010). `close_on_resolution` runs, credits **+$29.92 fictional profit** to live PM (`current_capital` 100.02 → 129.94). |
| 05:25 UTC | Operator triggers halt; service stopped, shadow_only=true set, restarted. |

**Live state at halt**:
- Wallet USDC.e: **$100.02** (untouched since deploy — zero real trades)
- Live PM `current_capital` (in-memory, dashboard): **$129.94** (fictional)
- `scalars.current_capital` (persisted): **$70.01** (post-deploy deduction, not updated by close_on_resolution — separate scalars-drift issue)
- Shadow portfolios: B/C/D/E at **$90/$80/$70/$50** with phantom open positions on the same constraint
- Telegram log: 1 ProactiveExit alert was sent (the FIRST one before INC-020 logic refused) per legacy code path; subsequent ~420 attempts were silent. Resolution event: no Telegram (live PM resolution doesn't currently fire a notification — separate gap).

**Six interlocking bugs**:

1. **CLOB schema mismatch** (root). `executor::submit_to_clob` posts an order
   payload that Polymarket now responds to with `order_version_mismatch`.
   Polymarket changed the schema (likely a new version field or a change to
   the EIP-712 typed data). We do not currently know what the new shape is —
   needs investigation against py-clob-client. Until fixed, every live order
   submission fails. Live execution is non-functional.

2. **No entry rollback on executor failure**. `enter_position` runs *before*
   `execute_arb`. When all legs reject, the paper position remains. PM
   bookkeeping diverges from wallet on the very next state-changing event
   (resolution, close_on_resolution, etc.). The phantom $29.92 was a direct
   consequence: position created on paper, market resolved, paper accounting
   booked the win, wallet untouched.

3. **Resolution closed paper-only position without verifying real shares**.
   `close_on_resolution` doesn't ask the executor "do we actually have
   terminal-Confirmed fills for these legs?" — it trusts PM state. Should
   refuse to settle if no real shares ever filled.

4. **Strategy tracker not notified on live API resolution**. When live PM
   resolves via `check_api_resolutions`, shadow strategies with the same
   constraint open keep their open-position rows. That's why the Strategies
   tab kept showing the trade as live even though Positions had closed it.

5. **INC-020 exit path uses empty `leg.token_id`**. By design, `MarketLeg.token_id`
   is `""` until populated by fill_tracker after real fills come back. Entry
   path correctly looks up `m.no_asset_id` from `engine.constraints` registry;
   my INC-020 path (added 2026-04-27) used `leg.token_id` directly. Even if
   bug 1 were fixed and entry succeeded, exit would still fail the executor's
   instrument lookup with "Unknown token_id".

6. **UMA dispute window ignored on close**. Resolution closed at 03:38 with
   `uma_status:proposed`. UMA proposals can flip during the dispute window;
   booking profit before finalisation can leave the books inconsistent if
   the resolution is overturned.

**Halt action (commit pending — this session)**:

1. `data/state_rust.db` and `config/config.yaml` snapshotted to forensics paths.
2. `live_trading.shadow_only` flipped `false → true` in config.yaml.
3. Service restarted with reason tag `inc021_halt_clob_schema_mismatch`.
   Boot log confirms: `[G1] execute_orders=true but shadow_only=true — live executor NOT started (safety)`.
4. Phantom $29.92 not yet reset; left in place until remediation cycle so the
   forensic chain is intact.

**Remediation plan (to land before re-enabling live)**:

| # | Fix | Priority |
|---|-----|----------|
| 1 | Investigate Polymarket order schema change. Hit /order with curl + reproduce. Diff against py-clob-client current. Update `executor::submit_to_clob` payload. | **P0 blocker** |
| 2 | Add entry rollback: when `execute_arb.all_accepted` is false, undo `enter_position` (or never enter unless the executor accepts). | P1 |
| 3 | `close_on_resolution`: refuse to settle if no terminal-Confirmed CLOB fills exist for this position (or no real shares are tracked in the wallet). | P1 |
| 4 | Wire `check_api_resolutions` resolutions to also notify `strategy_tracker` so shadow positions close in sync. | P2 |
| 5 | INC-020 exit path: pull `no_asset_id` / `yes_asset_id` from `engine.constraints` registry like the entry path does, instead of trusting empty `leg.token_id`. | P1 (bundle with #1) |
| 6 | Hold off on `close_on_resolution` while `uma_status == proposed`; require `resolved`/`settled` finality. Add timeout for stuck UMA proposals. | P2 |

**Already-shipped helpful infrastructure (this session, commit pending)**:

- **API-change tracker** (`executor::classify_clob_rejection` +
  `Orchestrator::alert_on_api_contract_rejection`): scans every CLOB rejection
  for schema/version/field/format/auth-drift keywords and fires a one-shot
  Telegram alert per (category, code, msg-snippet) per boot. Categories:
  `schema_version_mismatch`, `field_drift`, `format_drift`, `deprecation`,
  `auth_drift`. Means the next API change won't go undetected for hours.
  Wired at all three execute_arb call sites (entry, replacement, INC-020 exit).

**Lessons**:

- "Live executor armed" log line was true but *useless* — the executor was
  running but every single submission was failing with the same schema error
  for hours. Need a "live trades successfully landing" health metric, not
  just "executor not None". Could be: `time-since-last-Confirmed-fill > 24h`
  while opportunities are firing → alert.
- Paper bookkeeping and live execution diverge silently when entry isn't
  atomic. Next architectural pass should make the "create position" and
  "fill submitted to venue" boundary explicit — perhaps with a `Pending` state
  that only promotes to `Open` when at least one leg reaches `Submitted`.
- Phantom money is worse than no money: it makes the dashboard lie about
  ROI, contaminates strategy-tracker comparisons, and erodes the operator's
  trust in displayed numbers. The fictional $29.92 has to be reset before
  any further trading or the win-rate stats are corrupt.

---

### INC-021 update (2026-05-01 / 02): V2 port worked, auth + bug 7 surfaced

After the V2 port shipped (commit `8c31a29`) and the $5 probe configuration
(commit `ccfc26d`), the first live opportunity fired at **2026-05-01 17:18 UTC**:
3-leg `mutex_sell_all` on `mutex_0x50aba305…`, expected 6.94%. Shadow-B/C/D/E
all entered and won. Live attempted entry; outcome:

- ✅ **V2 schema is correct.** Polymarket no longer returns
  `order_version_mismatch`. The wire payload includes `signatureType: 1`,
  `timestamp`, `metadata` / `builder` (zero bytes32), `postOnly: false`,
  `deferExec: false`, no taker/nonce/feeRateBps. The full V2 envelope is
  on the wire and accepted at the schema layer.
- ❌ **All 3 legs rejected with `HTTP 401 {"error":"Unauthorized/Invalid api key"}`.**
- ✅ **Phase A bug 2 entry rollback fired correctly:**
  `INC-021 rollback: removed paper position … (3 legs, $5.00 restored)`.
  Position cleanly removed from open_positions, PM scalar capital restored.
- ❌ **Bug 7 (NEW): rollback didn't reverse the accounting journal.**
  `enter_position` writes BUY entries to the journal (debit Position+Fees,
  credit Cash) BEFORE the executor runs. The rollback restored the PM
  scalar but left the journal entries. The next USDC monitor tick fired
  `[ERROR] USDC drift: on-chain=$100.02 vs accounting=$95.02 ($5.00 drift)`.
- ❌ **Reactive `classify_clob_rejection` didn't match** "Unauthorized" /
  "Invalid api key" — heuristics only covered `signature_invalid` /
  `hmac_invalid`. The 401 storm was silent on Telegram.

**Fixes (commit `8c84eb9`)**:

1. **Bug 7 — `AccountingLedger::reverse_buy_by_position`**: posts mirror
   entries (credit Position, debit Cash, credit Fees) for the rejected
   BUY. `Engine::rollback_paper_entry` now calls it. Audit trail preserved
   (REVERSAL rows visible in journal).
2. **`classify_clob_rejection` extended** with `unauthorized`,
   `invalid api key`, `api_key_expired`, `invalid passphrase` patterns
   under category `auth_drift`. Future 401 storms fire one-shot Telegram.
3. **`TRADER_FORCE_NEW_CLOB_KEY=1` env var** override: forces
   `create_api_key` (POST /auth/api-key) at startup instead of derive-first.
   Used after a venue-side key rotation where derive returns a stale row
   that subsequently 401s on /order. Set once via systemd drop-in:
   `/etc/systemd/system/prediction-trader.service.d/force-new-key.conf`.

**Re-armed 2026-05-01 23:05 UTC** with full state reset + force-new-key.
The fresh `create_api_key` returned the same `c5a8fcd5…` prefix as the
rejected key, which means either (a) Polymarket's create endpoint is
idempotent on `wallet → key`, or (b) L2 auth isn't actually the root
cause. The next probe trade will tell us:
- 401 again → auth scheme changed in V2; need fresh investigation
  (possibly POLY_ADDRESS / POLY_API_KEY header format changed)
- different error → investigate that
- success → the issue was a transient venue cache, now warm

**Lessons**:
- Rollback isn't atomic across subsystems by default. Any "undo entry"
  primitive needs to undo PM scalar AND ledger AND any other
  side-effect writers (suspense, fill_quality, etc.). Worth a full audit
  pass before next live re-arm.
- Telegram alert categories should err on the side of catching MORE error
  patterns. A silent 401 storm cost us 2+ hours of observation. The
  classifier should categorize anything matching common HTTP-failure
  patterns into one of {schema/field/format/deprecation/auth/rate-limit}
  rather than fall through to "uncategorized" silently.
- "Idempotent" auth endpoints can mask a key-rotation bug: derive-first
  returns the existing row; create-on-fail never fires; if the key is
  bad, we never recover. Forcing create unconditionally on a rotation-class
  401 should probably be the default.

---

### INC-021 final findings (2026-05-02): V2 uses a wrapped collateral token (pUSD)

After fixing the negRisk domain name (commit `a4efdcd`, then reverted —
the actual SDK uses one name) and the signatureType (`1` → `0`, commit
`438e93f`), D2 reached the contract layer. New rejection:

```
HTTP 400 {"error":"not enough balance / allowance: the balance is not enough -> balance: 0, order amount: 2574250"}
```

**Discovery**: V2 trades against a NEW wrapped collateral token, not USDC.e.

| Token | Address | Source |
|------|---------|--------|
| **pUSD ("Polymarket USD")** | `0xC011a7E12a19f7B1f670d46F03B03f3342E82DFB` | py-clob-client-v2 1.0.1rc1 `config.py` `collateral` field |
| USDC.e (our monitor checks this) | `0x2791bca1f2de4661ed88a30c99a7a9449aa84174` | Polygon canonical USDC.e |

The wallet `0x37B2c35944aF8bE382F5Efc18a4555E1eE25A105` has $100.02 USDC.e
but **0.00 pUSD**. Every V2 BUY checks pUSD balance/allowance, sees 0,
and rejects. This explains 100% of the V2 failures since the schema port
landed.

**py-clob-client-v2 1.0.1rc1 release notes** (published TODAY, 2026-05-01):
`feat: add deposit wallet order support by @suhailkakar` (PR #39).
This is exactly the V2 inline-wrapping feature: you submit an order with
a `deposit_wallet`-flavored OrderType and the contract auto-wraps the
USDC.e on the fly. Examples added:

- `examples/account/approve_allowances.py`
- `examples/account/approve_neg_risk_allowances.py`
- `examples/account/get_balance_allowance.py`
- `examples/account/update_balance_allowance.py`
- `examples/orders/gtc_limit_buy_deposit_wallet.py`

**Three options to unblock**:

1. **Manual wrap via Polymarket UI** — operator logs into the web UI
   with the wallet, deposits some USDC.e (auto-wraps to pUSD),
   re-probes our $5 D2. Fastest path to validate everything else.
2. **Implement deposit_wallet OrderType** — port the V2 SDK pattern
   into our executor. Adds a new OrderType variant; build_order +
   submit_to_clob check the right pUSD-vs-USDC.e source.
3. **Implement wrap step in code** — add an on-chain
   `pUSD.deposit(usdc_amount)` raw transaction signer. Bigger lift
   (raw EIP-1559 signing, not EIP-712). One-time per top-up.

**State after this discovery**:
- `shadow_only: true` (halted)
- `TRADER_FORCE_NEW_CLOB_KEY` systemd override removed (daily refresh
  resumes normal flow on next start)
- All Phase A bug fixes (entry rollback, journal reversal, phantom
  payout reversal, token_id from constraints) confirmed working in
  the May 1 17:18 + 23:11 + 23:18 + 23:24 probe cycles
- V2 schema, signature, L2 auth all confirmed working

**Net status**: not a code bug — a missing on-chain prerequisite
(USDC.e → pUSD wrap). All deeper plumbing is correct.

**Lessons**:
- The agent's earlier V2 spec (`signatureType: 1`, `Polymarket Neg Risk
  CTF Exchange` domain name) was speculative and wrong on both counts.
  Always fetch the actual SDK source code at the named tag — the
  authoritative file is `ctf_exchange_v2_typed_data.py` + `signature_type_v2.py`,
  not a third-party cheatsheet or LLM summary.
- Migration cliff bigger than the wire-format change. Polymarket V2 isn't
  just a schema rev — it's a new collateral token. We caught it by
  probing live (HTTP 400 with the explicit error message). A purely
  static schema diff would have missed this.
- Probe-trade pattern (D2: GTC at off-market + cancel) is invaluable for
  surfacing layered failures fast. Got us from "401 unauthorized" to
  "invalid signature" to "balance: 0" in under an hour, vs. waiting
  hours for organic opportunities to fire.

---

### INC-021 SOLVED (2026-05-02): V2 onboarding complete — first live order accepted

After fixing signatureType (`1` → `0`), reverting the spurious negRisk domain
name, wrapping USDC.e → pUSD via KyberSwap, and setting all 6 V2 allowances,
both BUY and SELL paths are validated end-to-end on Polymarket V2.

**First successful V2 order**: `0x99270c39556857e8f3c6d73bb28bf878ee7379faf5cbb2ee400c10528ad6f21a`
(GTC BUY 1.62 SPURS YES @ $0.01, placed 2026-05-01 23:40:38 UTC, cancelled
~5s later). **First successful V2 SELL**:
`0x4d99ecf57d51b8532a3df03ce54524710335eb1754963303821eaec3066c033b` (12.31
shares of SPURS YES → $1.94 pUSD, 23:52:36 UTC).

#### Why every prior attempt failed (V1 → V2 migration cliff)

The D1-D9 milestone tests passed in March 2026 against V1 contracts, V1
schema, and USDC.e direct collateral. Polymarket cut over to V2 on
**2026-04-28**, and every single piece changed:

| Layer | V1 (D-test era) | V2 (now) | Failure we hit | Fix commit |
|------|-----------------|----------|---------------|----------|
| EIP-712 schema | 12-field struct (taker/nonce/expiration/feeRateBps) | 11-field (timestamp/metadata/builder added; old 4 removed) | `order_version_mismatch` | `8c31a29` |
| Domain version | "1" | "2" | (caught at port time) | `8c31a29` |
| Contract addresses | `0x4bFb41…/0xC5d5…` | `0xE1111800…/0xe2222d…` | (caught at port time) | `8c31a29` |
| signatureType for EOA | 0 | 0 (UNCHANGED — earlier agent report wrong) | `400 invalid signature` | `438e93f` |
| Domain name | `"Polymarket CTF Exchange"` (single) | same single (UNCHANGED — earlier agent suggestion wrong) | `400 invalid signature` | `438e93f` (revert) |
| Collateral token | USDC.e (`0x2791bc…`) directly | wrapped **pUSD** (`0xC011a7…`) — proxy, totalSupply 315M | `balance: 0` | wrap script `wrap_usdce_to_pusd.py` |
| BUY spender (negRisk) | V1 Exchange | NegRiskAdapter (`0xd91E80…`) | `allowance: 0 spender: 0xd91E80…` | wrap script step 5b |
| SELL spender (negRisk) | V1 Exchange (CTF approval) | NegRiskAdapter for CTF setApproval | `allowance: 0 spender: 0xd91E80…` (on SELL cleanup) | one-off `setApprovalForAll(CTF, Adapter, true)` |
| API key store | unchanged (L1 derive) | **keys re-issued at cutover** | `401 Unauthorized/Invalid api key` | `TRADER_FORCE_NEW_CLOB_KEY=1` env override (`8c84eb9`) |

#### V2 wallet onboarding (one-time per wallet)

Required transactions (encoded in `scripts/ops/wrap_usdce_to_pusd.py`):

1. `USDC.e.approve(KyberSwapRouter, $5)` — DEX router spend allowance
2. KyberSwap aggregator swap: `$5 USDC.e → ~$4.97 pUSD` (~0.5% slippage; routed via Uniswap V4)
3. `pUSD.approve(V2 CTF Exchange, MAX)` — for buys on standard markets
4. `pUSD.approve(V2 NegRisk Exchange, MAX)` — for buys on negRisk markets
5. `pUSD.approve(NegRiskAdapter, MAX)` — actual ERC-20 spender on negRisk
6. `CTF.setApprovalForAll(V2 CTF Exchange, true)` — sells on standard
7. `CTF.setApprovalForAll(V2 NegRisk Exchange, true)` — sells on negRisk
8. `CTF.setApprovalForAll(NegRiskAdapter, true)` — sells on negRisk **(missed in initial run; surfaced when cleanup SELL hit `allowance: 0` on adapter)**

Total gas: ~0.02 POL (~$0.005). Total swap slippage: ~$0.025 on $5.

#### Telegram-side observations

The reactive `classify_clob_rejection` heuristic was extended in commit
`8c84eb9` to also match `unauthorized`, `invalid api key`,
`api_key_expired`, `invalid passphrase` — auth_drift category. Without that
extension, the May 1 17:18 401 storm went silent. With the extension, any
future 401 fires a one-shot Telegram alert per (category, code, msg-snippet)
per boot.

#### Final V2-ready state (Dublin, 2026-05-02)

Wallet `0x37B2c35944aF8bE382F5Efc18a4555E1eE25A105`:

- **USDC.e**: $95.02 (operator reserve)
- **pUSD**: $2.24 (depleted from probe testing — top up via wrap script if needed)
- **All 6 V2 allowances**: MAX / True

Engine config:
- `shadow_only: false` (LIVE EXECUTOR ARMED)
- `min_trade_size: 3.0`, `capital_per_trade_pct: 0.03`, `max_positions: 1`
- Trade base size: $3 (fits inside pUSD with fee headroom)

Code state (commit `438e93f` plus uncommitted `scripts/ops/wrap_usdce_to_pusd.py`):
- V2 EIP-712 + envelope correct
- All Phase A bug fixes (entry rollback, journal reversal, phantom payout
  reversal, INC-020 token_id from constraints) confirmed working in 4+
  probes
- Reactive API-change tracker + proactive header monitor + GitHub release
  poll all running

**Status**: cleared for live. The next mutex arb opportunity that fires
will exercise the full V2 BUY → fill → resolution → SELL chain. Phantom
states + drift are protected by the bug-7 journal-reversal fix and the
phantom-payout reversal sanity check.

**Outstanding (non-blocking)**:
- Non-negRisk path validation pending — Gamma's `?negRisk=false` query
  filter is unreliable (consistent with INC-019 finding) so D2's
  `find_liquid_market()` keeps selecting a misclassified negRisk market.
  Patched client-side filter on Dublin (`clob_test/src/clob_client.rs`
  added `&& m.neg_risk == want_neg_risk` clause) but Gamma's per-market
  `neg_risk` field in the list response is itself flipped, so the filter
  still doesn't catch. Path forward: hand-pick a known non-negRisk market
  and probe it directly. The V2 Exchange + pUSD allowance + CTF
  setApproval are all in place for that path so no additional onboarding
  is needed.
- Wrap script second-run regression: when run a second time on the same
  KyberSwap quote, gas estimation reverts with `insufficient funds for
  transfer`. Likely route caching at KyberSwap. Workaround: regenerate
  the quote fresh each run (the script does this — but the gas estimate
  on the swap tx still failed once). Not blocking; can be addressed if
  another wrap is needed.

**Lessons**:
- LLM-summarised SDK specs are wrong often enough that you have to fetch
  the actual source. Both the `signatureType: 1` and the
  `"Polymarket Neg Risk CTF Exchange"` domain name guesses were wrong; the
  authoritative source is `ctf_exchange_v2_typed_data.py` +
  `signature_type_v2.py` in the V2 SDK at the released tag.
- Probe-trade pattern (D2: GTC at off-market + cancel) surfaces layered
  failures fast — got us through 6 distinct error codes in ~30 minutes
  vs. waiting hours for organic opportunities.
- Migration cliffs are bigger than the wire-format change. Polymarket's V2
  isn't just a schema rev — it's a new collateral token, a new spender
  contract for negRisk routing, a new key issuance, and a new EIP-712
  domain. Every layer needs verification independently.
- For geoblocked operators, on-chain DEX routing (via KyberSwap or
  similar aggregator) is a reliable substitute for the venue's web UI.
  ~30 minutes total for the first-time setup.

---

### INC-020: Proactive Exits Did Not Submit Real CLOB Sells — Phantom-Proceeds Latent Bug (2026-04-27)

**Severity**: CRITICAL (would have caused immediate accounting/wallet divergence on first live arb post-INC-019 fix)
**Status**: FIXED — discovered + fixed in the same session, never bit live because pre-INC-019 the gamma_freshness false-positive prevented all live entries

**Summary**: When the live engine detected a proactive-exit opportunity (liquidation
value ≥ 1.2× resolution payout), it called `engine.liquidate_position()` —
which only does paper bookkeeping. **No CLOB sell order was ever submitted.**
The accounting credited `current_capital += net_proceeds` and moved the
position to `closed_positions`, but the actual ERC-1155 share tokens stayed
in the wallet. At resolution they would have paid out (or not) on top of the
already-credited "exit proceeds", double-counting on wins and producing
phantom losses on losers.

**Detection**: User asked whether early exits were calibrated. Investigation
of `orchestrator.rs::check_proactive_exits` (line 2941) revealed the call
chain: `→ engine.liquidate_position` → `lib.rs:712` → only paper bookkeeping
+ accounting-ledger record from `current_bids` (top-of-book), no executor
call. Confirmed by reading the entire path from `check_proactive_exits`
through to the CLOB submit primitives — the executor's `execute_arb` is
fully sell-capable and present, just not wired here.

**Why it never bit production**: Live capital sat at $100 with zero open
positions throughout 2026-04-23 → 2026-04-27 because INC-019's
gamma_freshness false-positive blocked every entry. So the proactive-exit
code never had a real position to mishandle. The bug would have manifested
on the first arb entry following the INC-019 fix shipped earlier the same
day (commit `de3cf09`).

**Historical context — the 78% / 74% data**: SQLite snapshot
`state_rust.db.bak` (Mar 22) shows 32 closed positions from earlier
paper-trading runs: **25 (78%)** closed via `proactive_exit` at avg **74.33%
profit**, **7 (22%)** via `resolved` at avg 7.97%. That data is from shadow
/ paper mode where the "sold at top bid" model is the *intended* abstraction
— not from a real wallet. So the 350% portfolio growth recorded in earlier
backups was a *valid simulation* result, not real money.

**Fix (commit pending — this session)**:

1. **`rust_engine/src/position.rs`** — added `ExitLegFill` and `ExitOutcome`
   types plus `PositionManager::apply_exit_fills(position_id, &[ExitLegFill],
   reason)`. Reduces per-leg shares by ACTUAL filled amounts, credits ACTUAL
   proceeds (filled_shares × fill_price − fees, not bid×shares), and only
   moves the position to `closed_positions` when every leg has reached zero
   shares. Partials reduce the position in place; below-min residuals hold
   to resolution.

2. **`rust_engine/src/lib.rs`** — added `Engine::apply_exit_fills` (wraps
   the PM method + records ACTUAL fills on the accounting ledger via
   `record_sell_dedup`) and `Engine::execute_position_exit(executor, …)`
   (translates `(market_id, token_id, bid, shares)` legs into FAK SELL
   orders via the existing `Executor::execute_arb`). Added a doc comment to
   the legacy `liquidate_position` warning callers it's paper-only and
   must not be used in live trading contexts (except `reason="resolution"`
   where the platform itself settles the tokens).

3. **`rust_engine/src/executor.rs`** — added
   `Executor::sell_orders_for_position(pid)` returning per-leg
   `(market_id, filled_qty, avg_fill_price, terminal)` so the orchestrator
   can reconcile from real fills after the FAK window closes.

4. **`rust_supervisor/src/orchestrator.rs::check_proactive_exits`** — split
   live vs. shadow paths:
   - **Live** (`!shadow_only && executor.is_some()`): collect per-leg
     `(mid, token, bid, shares)`; skip below-min legs (logged); submit
     FAK SELLs via `engine.execute_position_exit`; poll
     `executor.sell_orders_for_position` up to ~3s (six 500ms ticks); apply
     actual fills via `engine.apply_exit_fills`. If any phase produces
     zero submittable legs or zero fills, the position is **left open** —
     no phantom proceeds credited.
   - **Shadow / paper-only**: legacy `engine.liquidate_position` retained;
     same behaviour as before because there are no real shares to sell.

**Pre-live testing limits acknowledged**: this is a money-handling
correctness fix that can't be fully validated without a real arb entering
live AND the bid then crossing the 1.2× exit gate. Tests cover the unit
behaviour of `apply_exit_fills` (full close, partial close, zero fills)
but the end-to-end CLOB submission path requires real-money observation.
Mitigations: (a) safety guard in `liquidate_position` doc comment, (b)
explicit "no fills → position holds" branches in the orchestrator, (c)
Telegram alert on every exit submission so the operator can spot-check
against the wallet.

**Lessons**:

- "It was tested in shadow" is not the same as "it was tested live." Shadow
  / paper modes use idealised execution models that are fine *as models*
  but mask real-execution gaps. Any code path that touches real money needs
  a separate review checking the executor is actually invoked, not just
  the bookkeeping.
- Function names like `liquidate_position` create strong implications
  ("liquidate = sell"). The legacy method only ever did bookkeeping and
  ledger-recording — should probably be renamed `book_paper_liquidation`
  or similar. Filed as a follow-up rename.
- Symmetric instrumentation (entry vs. exit) is missing. Entries go through
  `executor.execute_arb` which is well-tested. Exits had no equivalent
  test coverage of the actual submission path. The new tests cover the PM
  bookkeeping; exec-path tests should follow once the live deploy has
  shaken out edge cases.

**Open follow-ups**:
- **PROACTIVE_EXIT_MULTIPLIER calibration** (raised by user): the 1.2 value
  is arbitrary. The historical 25 proactive exits captured an avg 74%
  profit at the 1.2 gate, which strongly suggests we're exiting too early
  on average. Calibration needs (a) value-curve recording (`ratio` over
  time per position) and (b) book-depth recording (both bid and ask, 5
  levels each) to model the liquidity-adjusted exit ratio achievable at
  full position size. Filed as a separate work item.
- **Rename** `PositionManager::liquidate_position` → `book_paper_liquidation`
  and `Engine::liquidate_position` similarly, to make the safety contract
  obvious from the name. Keep `apply_exit_fills` as the live-path API.

---

### INC-019: gamma_freshness 100% False-Reject — 11 Live Trades Missed (2026-04-27)

**Severity**: HIGH (revenue impact — 5 winning shadow-D trades worth +10.16% missed live across 4 days)
**Status**: ROOT-CAUSED — fix in flight

**Summary**: For ~4 days starting 2026-04-24, every negRisk-group mutex arbitrage that
the scanner correctly identified as a structurally-exhaustive 3-leg group was
**rejected at the live entry gate** by the runtime `gamma_freshness::check_group_freshness`
check. Shadow strategies (which skip this check via `!self.cfg.shadow_only`) entered
and won. Live capital stayed at $100. Shadow-D (strategy that mirrors live's gates)
grew to $110.16 — the exact 10.16% the user noticed at 11:33 UTC 2026-04-27.

**Detection**: Operator asked "why did we miss 5 trades and 10% growth (live vs strat d
shadow)?". SQL on `evaluated_opportunities` showed 11 rows with
`entered=0`, `rejected_reason=NULL`, `strategy_accepted='Shadow-B,Shadow-C,Shadow-D,Shadow-E'`
— shadows took them, live didn't, and no reason was logged. `journalctl` grep for
`SKIP \(gamma_freshness` matched all 11 timestamps with the same string:
**`reason=group_grew:3->100`** for every single one.

**Root Cause**: `rust_engine/src/gamma_freshness.rs:91-94` queries
```
GET https://gamma-api.polymarket.com/markets?negRiskMarketID={id}&limit=100
```
and counts the returned array length. Polymarket's Gamma API does **not actually
filter** by `negRiskMarketID` — the same broken behaviour we documented in INC-017's
investigation. The endpoint just returns the first 100 markets in its catalog. So
`current` is always exactly 100 (the page-size cap), `expected` is the recorded
`full_group_size` (typically 3-12), and the verdict is always `GroupGrew { 100, expected }`.

This was a single-line conceptual error: the function assumed Gamma's
`?negRiskMarketID=` filter worked. INC-017's investigation explicitly noted it
doesn't, and the structural fix (using `constraint.full_group_size` recorded at
detection time, in `eval.rs`) was deployed correctly. The runtime gamma_freshness
check was added at the same time (v0.20.3, 2026-04-21) but never validated against
the same broken endpoint, and quietly burned every live opportunity since.

**Why shadow worked**: `orchestrator.rs:2433` skips the freshness call entirely when
`self.cfg.shadow_only`. But the `strategy_tracker` shadow path is a *different*
mechanism — it operates on the same `Opportunity` stream that the live executor sees,
*before* the orchestrator's per-opp gates run. So the shadow strategies bypass the
gamma_freshness gate by virtue of being upstream of it, not by `shadow_only`. Net
effect: same — shadows ran the trades, live didn't.

**Why we trust shadow over the freshness check**: Shadow-D entered all 5 of these
"group_grew" opportunities and won 5/5. If the group had genuinely expanded to 100
outcomes between detection and entry, those YES/NO tokens would have been worthless
or near-worthless. The 5/5 win record is direct evidence that `full_group_size=3`
captured at detection time was correct, and that `gamma_freshness` returned a
spurious "100" every time.

**Impact**: 11 evaluated opportunities silently skipped. 5 of them were trades
shadow-D won — gross expected profit ~$5–10 each at $30 capital_per_trade (4-9% on
$30 ≈ $1.20–$2.70/trade). Total missed live profit: roughly **$10 over 4 days**, or
the entire 10.16% gap visible on Shadow-D vs live.

**Forensic data** (preserved at `docs/forensics/inc019_pre_wipe_20260427_103322Z.txt`
and `data/forensics/inc019_pre_wipe_20260427_103322Z.txt` on Dublin):

```
Pre-wipe strategy_portfolios @ 2026-04-27 10:33:22 UTC:
  Shadow-A    100.00   entered=0  W/L=0/0   evals_seen=10  rejected=10
  Shadow-B    103.31   entered=5  W/L=5/0   evals_seen=10  rejected=5
  Shadow-C    106.69   entered=5  W/L=5/0   evals_seen=10  rejected=5
  Shadow-D    110.16   entered=5  W/L=5/0   evals_seen=10  rejected=5
  Shadow-E    116.28   entered=4  W/L=4/0   evals_seen=10  rejected=6
  Shadow-F    100.00   entered=0  W/L=0/0   evals_seen=10  rejected=10

Live PM:        100.00 (initial)  open=0  closed=0  trades=0
```

**Fixes**:

1. **gamma_freshness.rs**: detect `current == limit` as a broken-filter symptom
   and return `Verdict::Ok` with a warn-level log, instead of `GroupGrew`. The
   `eval.rs` `full_group_size` check at evaluation time remains the authoritative
   exhaustiveness gate (deterministic, uses scanner-time data).
2. **orchestrator.rs**: every live-only silent-skip path (`gamma_freshness`,
   `negRisk_cap_full`, `InsufficientCapital`) now writes `rejected_reason` into
   `evaluated_opportunities`. No more invisible rejections.
3. **Shadow portfolios reset to $100 / no history** so post-fix comparisons are
   apples-to-apples. Pre-wipe state preserved in forensics file above.

**Lessons**:

- Any opportunity-level decision that doesn't write to `evaluated_opportunities`
  is a forensic black hole. The "rejected_reason hardcoded None" gap from INC-018
  was supposedly fixed yesterday, but only at *one* log site — three other live
  silent-skip paths still bypassed the table.
- "Same broken thing in two places" — INC-017's broken `?negRiskMarketID=` was
  fixed in `eval.rs` but missed in `gamma_freshness.rs`. When fixing a vendor-API
  defect, grep for *every* call site of the bad endpoint, not just the first one
  encountered.
- Live-vs-shadow capital divergence is the canonical regression signal. Daily
  delta-check should be a dashboard widget — would have caught this within hours
  instead of days.

---

### INC-018: Shadow/Live Divergence — Hull City Startup-Race Miss (2026-04-23)

**Severity**: LOW (first observation; no capital impact — live correctly abstained)
**Status**: OPEN — reactive-fix policy adopted

**Summary**: 75 seconds after rearming on the newly-rebuilt Dublin VPS, a mutex-arb
opportunity on a Hull City 3-way fired. The live executor correctly SKIPPED
(`SKIP (no book): mutex_0xda4e7ddcec2ea870744128... asset 2053143248550204 has no
book data`) because one of the three WS order books had not yet populated after
restart. However, the strategy tracker (Shadow-D et al.) entered the position
anyway, using something other than the live WS book (likely the Gamma REST
snapshot or a stale fetch). Shadow portfolio booked an entry that live physically
could not.

**Detection**: Operator noticed shadow portfolios showed open positions after
arming; forensics on `journalctl` found the single `SKIP (no book)` line at
10:46:15.628 UTC, 75s after the 10:45:00 UTC armed restart.

**Root Cause**: `strategy_tracker` evaluation path does not enforce the same
book-freshness gate that `eval.rs` applies on the live path. When any of the
3 legs has no WS book data, live refuses but shadow proceeds on best-available
pricing.

**Impact**: Shadow P&L systematically overstates what live could execute during
any window where WS book coverage is incomplete (restarts, new hot-constraint
promotions, shard disconnects). Worst during the first ~1–2 minutes after a
service restart. No direct capital impact; the safety behaviour of the live
path is correct.

**Mitigation (initial wipe)**: Wiped `strategy_open_positions` /
`strategy_closed_positions`, reset all 6 strategy portfolios to $100, restarted
armed at 10:49:33 UTC. WS shards had fully converged by that point
(7938/7938 assets, all ~584 hot constraints with book data), so the
book-staleness race could not recur until the next restart.

**Update 2026-04-23 ~15:45 UTC — second-occurrence investigation**:
After the 10:49 wipe, four new shadow entries appeared within the next ~5 hours,
none of which entered live:

| t (UTC)  | constraint                          | method          | profit% | accepted_by       |
|----------|-------------------------------------|-----------------|---------|-------------------|
| 12:54:33 | `mutex_0x751f4f67…` Horníček/Atubolu "most clean sheets" | `mutex_sell_all` | 5.25%   | Shadow-B, C, **D** |
| 15:08:07 | `mutex_0xf6139c35…` Argentina vs Algeria 3-way            | `mutex_buy_all`  | 4.16%   | Shadow-B          |

Forensics:
- The Horníček/Atubolu one hit `SKIP (unrepresented outcome)` in live at
  12:54:33.268. The AI resolution validator correctly flagged that "most
  clean sheets in Europa League" has ~20+ candidate goalkeepers, not 2 —
  the AI answer was semantically correct. **However, the orchestrator's use
  of that flag was wrong**: it rejected for both buy_all and sell_all, but
  per INC-017 lessons unrepresented outcomes are *catastrophic* only for
  buy_all. For sell_all (which this was), an uncovered winner pays all our
  NO tokens at $1 — the maximum-profit case. **Live's rejection was
  over-conservative — shadow-B/C/D taking this opp was the correct call.**
  We have been leaving valid sell_all arbs on the table since INC-017
  shipped.
- The Argentina/Algeria one has no `SKIP` line in the journal but did not
  enter live (`evaluated_opportunities.entered=0`, `rejected_reason=NULL`).
  Most likely rejected on depth or staleness at the execution tick; the
  reject-reason telemetry gap prevents a definitive call post-hoc.

This reveals TWO distinct issues:
1. **Shadow/live asymmetry** (the original INC-018 framing): shadow uses raw
   `eval.rs` output; live additionally applies `validate_opportunity` in
   `orchestrator.rs` which gates on depth (B1.0), book staleness (B1.3),
   unrepresented outcomes, and AI date bounds. Shadow bypasses all four.
2. **Wrong direction on `unrepresented_outcome` for sell_all**: the
   orchestrator gate was over-conservative — should only reject when
   `!opp.is_sell`.

**Fix (2026-04-23) — deployed in commit 35899c7, restarted 15:58:58 UTC**:
1. Extracted `quality_reject_reason(&self, opp) -> Option<&'static str>` from
   `validate_opportunity` — canonical reject tag for depth / no_book /
   stale_book / unrepresented_outcome / ai_date.
2. Strategy tracker feed now filters opportunities through
   `quality_reject_reason` before `process_opportunities`. Shadow and live
   are gated identically on quality; only holdings/inventory differ.
3. `evaluated_opportunities.rejected_reason` now populated from
   `quality_reject_reason` output instead of hardcoded `None`. Future
   shadow/live divergences leave a DB breadcrumb.
4. **INC-017 follow-on correction**: `has_unrepresented_outcome` now only
   rejects when `!opp.is_sell`. Sell_all with unrepresented outcomes is
   explicitly allowed — uncovered winner pays all NO tokens (max profit).
   The Horníček/Atubolu opp would have entered live under this fix.
5. Shadow state wiped and service restarted armed post-deploy.

**Lessons**:
- Shadow-path divergence is a two-way signal: it can reveal missing gates
  in shadow OR over-conservative gates in live. Always investigate which
  direction is wrong before "fixing" in one direction.
- AI flags should be interpreted in the context of the trade structure
  (buy_all vs sell_all). The flag `has_unrepresented_outcome` is a
  semantic property of the market; its trading implication depends on
  which side we're taking.
- Reject-reason telemetry matters. NULL `rejected_reason` rows in
  `evaluated_opportunities` are a forensic black hole.

**Fix Policy (adopted 2026-04-23)**: Divergences of this class are handled
**reactively**: as each occurs, (1) log it as a new INC entry, (2) fix toward
parity with live (Option B — shadow gates on same book-freshness/fresh-data
requirement as live), (3) wipe shadow state tables, (4) restart service armed.
Not chasing proactive coverage; waiting for real divergences to reveal which
gates matter.

**Lessons**:
- Shadow P&L is a strategy-comparison signal, not a forecast of live P&L,
  unless shadow and live share identical entry gates.
- Warm-up window (WS book convergence) is ~60–120s post-restart at current
  shard count; avoid drawing conclusions from shadow entries inside that window.
- The live path's strict "no book → no trade" gate is working correctly and
  should be the reference behaviour; any subsystem that tracks positions
  should match it.

---

### INC-017: Non-Exhaustive Mutex Arb — Hong Kong Temperature (2026-04-01)

**Severity**: MEDIUM
**Status**: RESOLVED

**Summary**: Shadow strategies A/B/C/D/F entered a `mutex_buy_all` arb on two Hong Kong temperature markets ("exactly 20°C on March 30" and "≤19°C on March 30"). The actual temperature was above 20°C — both YES tokens resolved to zero. Total shadow loss ≈ $808.52. Positions became zombies (never closed) due to a second bug: the resolution code had no handler for "all markets resolve NO."

**Detection**: Repeated log warning `Strategy constraint mutex_0x876e920c...: all markets resolved NO — closing with NONE winner` (after fix). Pre-fix: warning fired but position stayed open.

**Root Cause (Bug 1 — entry)**: `check_mutex_arb()` in `arb.rs` only checked that YES prices of *candidate markets* summed to < 1.0 — it did not verify the candidate set was exhaustive. The two HK markets are part of a larger condition group (~8 temperature bands). Markets for 21°C, 22°C, 23°C+ were not in the hot WebSocket set and were never fetched. The system assumed a 2-market group covered all outcomes.

**Root Cause (Bug 2 — resolution)**: `check_constraints_resolution()` in `lib.rs` only pushed a resolved entry when a YES winner was found. When all markets resolved NO, it logged a warning but left the position open forever (zombie).

**Fix**:
- Bug 1: Added `check_mutex_exhaustiveness()` — fetches full condition group from Gamma API at entry time, rejects if any non-candidate market has YES price > 0.005. Applied once per eval batch before strategy tracker and main PM both consume opportunities.
- Bug 2: `check_constraints_resolution()` now pushes sentinel `NONE` when all markets resolve NO. `buy_all` NONE → payout 0 (full loss); `sell_all` NONE → all NO tokens pay (max win). Both `position.rs` and `strategy_tracker.rs` handle NONE correctly by coincidence of their lookup logic.
- DB: 5 zombie positions manually closed as full losses post-deploy.

**Lessons**:
- Price-sum < 1.0 is *necessary* but not *sufficient* for a risk-free mutex arb — exhaustive coverage of the condition group is also required.
- Always handle the "all NO" resolution case explicitly; don't assume a YES winner must exist.
- sell_all non-exhaustiveness is benign (uncovered YES winner = all our NO tokens pay); buy_all non-exhaustiveness is catastrophic (uncovered YES winner = full loss).

---

### INC-016: Cross-Filesystem Git Blindspot — 172 Lines Uncommitted (2026-03-24)

**Severity**: HIGH
**Impact**: `rust_engine/src/lib.rs` had 172 lines of uncommitted changes (module declarations for `usdc_monitor`, `fill_confirmation`, `sports_ws`, plus reconciliation return type fixes) that were invisible to Windows git but visible to WSL git. The E3 commit deployed to VPS failed to build because the reconciliation functions returned tuples but lib.rs declared single-value returns. VPS was unable to build for ~30 minutes during the E4 launch window.
**Root cause**: The repo lives on WSL's ext4 filesystem, mounted into Windows via `\\wsl.localhost\Ubuntu`. Windows git (`git diff HEAD`) showed no changes because it resolves paths differently than WSL git. The file was modified in WSL sessions but only WSL's `git diff` detected the delta. All prior commits were made from Windows git, silently skipping the WSL-only changes.
**Detection**: VPS `cargo build --release` failed with `E0308: mismatched types` on `reconcile_startup` and `reconcile_periodic`. md5sum comparison between local and VPS confirmed different file content despite same git commit hash.
**Fix**: Committed the missing changes from WSL git and pushed. VPS pull + rebuild succeeded.
**Prevention**: Always run `git status` and `git diff` from WSL (not Windows) when working on a WSL-hosted repo. Consider adding a pre-push hook that runs `git diff HEAD` from within WSL to catch cross-filesystem blindspots.
**Lessons**: Never trust Windows git to see changes on a WSL ext4 mount. The `P:\` drive mount is for editor convenience only — all git operations should go through WSL.

---

### INC-015: Strategy-Only Positions Not Resolution-Checked (2026-03-29)

**Severity**: MEDIUM
**Status**: RESOLVED

**Summary**: Positions held exclusively by the strategy tracker (not in the main portfolio manager) were never checked for resolution via Gamma API. `check_api_resolutions()` only polled for positions in the main PM, so strategy-only virtual positions could remain open indefinitely after their markets resolved.

**Detection**: Wellington temperature and Montedio Yamagata positions stayed open 13+ hours after their events ended. Manual investigation revealed the resolution polling path never covered strategy-tracker-only positions.

**Root Cause**: `check_api_resolutions()` iterated only over positions in the main portfolio manager. The strategy tracker maintained its own independent set of virtual positions, but no code path polled Gamma API for their resolution status.

**Fix**: Extracted a shared Gamma resolution helper from the existing `check_api_resolutions()` logic. Added `check_strategy_resolutions()` that polls strategy-only constraint IDs on the same 5-minute interval.

**Data Note**: Wellington temperature market was manually resolved with the wrong winner (21C instead of 22C) before the automated fix was deployed. Auto-resolution later confirmed the correct outcome (22C). Minor historical data inconsistency.

**Lessons**: Any subsystem that independently tracks positions must have its own resolution polling path, or share the main one. Strategy tracker was added after the resolution system and was never wired into it.

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
