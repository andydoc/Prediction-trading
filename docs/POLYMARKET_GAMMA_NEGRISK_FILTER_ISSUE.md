# Draft GitHub issue — Gamma `?negRiskMarketID=` filter ignored

Filed at: TBD (operator to post under their GitHub account; do NOT post from
this repo's CI / bot identity).

Suggested target repo: `Polymarket/py-clob-client` (the original `/markets`
filter family lives in Gamma docs that are referenced from there) **or**
`Polymarket/agents` if a more general "gamma-api bug" tracker exists.
Cross-link to closed issue #334 (cursor ignored — separate bug, fixed
2026-04-26) for context.

---

## Title

`gamma-api: ?negRiskMarketID=<id>&limit=N saturates at limit, returns unrelated markets`

## Summary

Both `https://gamma-api.polymarket.com/markets?negRiskMarketID=<id>&limit=N`
and the V2 successor `/markets/keyset?negRiskMarketID=<id>&limit=N` return
**up to N markets that don't share the requested negRiskMarketID**. The
filter parameter appears to be silently ignored — the server returns the
first N markets in its catalog regardless of the value passed.

This breaks any client that uses the negRiskMarketID query parameter for
runtime exhaustiveness verification of a negRisk group (per the Polymarket
docs which list `negRiskMarketID` as a supported filter).

## Reproducer

Pick a known negRisk group of size 3 (a 3-leg mutex such as a football
3-way: home / draw / away). Get its `negRiskMarketID` from any one of the
three markets via `/markets/{id}`. Call:

```bash
NRMID=0x...   # 32-byte hex, the negRiskMarketID returned by /markets/{id}
curl -s "https://gamma-api.polymarket.com/markets?negRiskMarketID=${NRMID}&limit=100" \
  | python3 -c "import json,sys; d=json.load(sys.stdin); print(len(d), 'markets returned'); print('group nrmIDs:', set(m.get('negRiskMarketID') for m in d[:5]))"
```

Expected: 3 markets, all with `negRiskMarketID == NRMID`.
Actual: up to 100 markets, the first few have `negRiskMarketID` values
unrelated to `NRMID` (different groups entirely).

Same on the V2 keyset endpoint:

```bash
curl -s "https://gamma-api.polymarket.com/markets/keyset?negRiskMarketID=${NRMID}&limit=100" \
  | python3 -c "import json,sys; d=json.load(sys.stdin); m=d['markets']; print(len(m)); print(set(x.get('negRiskMarketID') for x in m[:5]))"
```

## Real-world impact

Custom client maintaining a runtime "freshness" check against a recorded
group size at constraint-detection time:

```rust
// fail-closed: if the live group size > recorded → reject entry
let resp = http.get(format!("/markets?negRiskMarketID={}&limit=100", id)).send()?;
let arr: Vec<Value> = resp.json()?;
if arr.len() > expected_size { reject("group_grew") } else { ok }
```

This rejected **100% of legitimate negRisk arb opportunities** for ~7 days
post-deploy because the response always contained `limit=100` markets,
regardless of how big the actual group was.

We worked around it by treating `arr.len() == limit` as a broken-filter
signal and falling back to the detection-time group size (recorded by our
scanner). Other clients without that fallback would silently fail-closed
on every legitimate negRisk trade.

## Suggested fix

- Make `?negRiskMarketID=<hex>` actually filter on the field
- OR document that the filter is unsupported on `/markets[/keyset]` and
  point clients at a different mechanism (a `/groups/{negRiskMarketID}`
  endpoint would be ideal)
- OR return an explicit `400 unsupported_filter` so clients can detect
  the misuse rather than silently getting wrong data

## Related

- Closed #334 (`cursor` parameter ignored on keyset endpoints) — distinct
  cursor-pagination bug, fixed 2026-04-26
- This filter bug is independent and still present as of 2026-05-05

## Environment

- 2026-05-05, gamma-api.polymarket.com from multiple IPs (Polygon validators,
  cloud VPS, residential)
- Python 3.12, curl, Rust reqwest — all see the same response shape
- `/markets/{id}` (single-market endpoint) returns the **correct**
  `negRiskMarketID` field per market — the server knows the value, the
  list endpoint just doesn't filter on it
