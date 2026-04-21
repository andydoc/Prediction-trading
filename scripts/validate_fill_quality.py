#!/usr/bin/env python3
"""
Fill quality validator (F-pre-4 / G4 — post-live analysis).

Reads data/fill_quality.log (JSON-lines) and joins intent + actual records
by opp_id. For each completed pair:
  - Computes per-leg fill ratio: (intended_price / actual_price) for buy
    legs, or (actual_price / intended_price) for sell legs.
    A ratio >= 1.0 means we filled at AT LEAST as good a price as expected.
  - Computes whole-trade ratio: weighted average of per-leg ratios by
    intended notional.
  - Compares whole-trade ratio against `min_profit_ratio` from config.yaml.

Acceptance threshold: 95% of completed trades must have whole-trade ratio
>= min_profit_ratio. Below that, alert.

Run weekly during the supervised period; will become a cron once we have a
stable baseline.

Usage:
    python3 scripts/validate_fill_quality.py [--log path] [--days N] [--min-ratio R]

Created 2026-04-21 for v0.20.3 (F-pre-4 / G4 infra).
"""
import argparse
import json
import sys
from collections import defaultdict
from datetime import datetime, timedelta, timezone
from pathlib import Path


def load_records(log_path: Path, since_ts: float):
    """Yield JSON records from the log, newer than since_ts."""
    if not log_path.exists():
        print(f"WARN: log file not found: {log_path}", file=sys.stderr)
        return
    with log_path.open("r") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                rec = json.loads(line)
            except json.JSONDecodeError as e:
                print(f"WARN: skipping malformed line: {e}", file=sys.stderr)
                continue
            if rec.get("ts", 0) >= since_ts:
                yield rec


def join_pairs(records):
    """Group records by opp_id; return {opp_id: (intent, actual_or_None)}."""
    intents = {}
    actuals = {}
    for r in records:
        opp_id = r.get("opp_id")
        if not opp_id:
            continue
        if r.get("kind") == "intent":
            intents[opp_id] = r
        elif r.get("kind") == "actual":
            actuals[opp_id] = r
    return {oid: (intents[oid], actuals.get(oid)) for oid in intents}


def compute_ratio(intent: dict, actual: dict) -> float:
    """Whole-trade fill quality ratio, weighted by intended notional."""
    intended_prices = intent.get("intended_prices", {})
    intended_bets = intent.get("intended_bets_usd", {})
    actual_prices = actual.get("actual_prices", {})
    is_sell = intent.get("is_sell", False)
    if not intended_prices or not actual_prices:
        return float("nan")

    weighted = 0.0
    weight_sum = 0.0
    for mid, ip in intended_prices.items():
        ap = actual_prices.get(mid)
        if ap is None or ap <= 0 or ip <= 0:
            continue
        ratio = (ap / ip) if is_sell else (ip / ap)
        notional = intended_bets.get(mid, 0.0)
        weighted += ratio * notional
        weight_sum += notional

    return weighted / weight_sum if weight_sum > 0 else float("nan")


def main() -> int:
    ap = argparse.ArgumentParser(description="Validate fill quality from JSON-lines log.")
    ap.add_argument("--log", default="data/fill_quality.log", type=Path,
                    help="Path to fill quality log (default: data/fill_quality.log)")
    ap.add_argument("--days", default=7, type=int,
                    help="Lookback window in days (default: 7)")
    ap.add_argument("--min-ratio", default=0.70, type=float,
                    help="Minimum acceptable fill ratio (default: 0.70 = live_trading.min_profit_ratio default)")
    ap.add_argument("--target-pct", default=0.95, type=float,
                    help="Required fraction of trades >= min-ratio (default: 0.95)")
    args = ap.parse_args()

    cutoff_ts = (datetime.now(timezone.utc) - timedelta(days=args.days)).timestamp()
    records = list(load_records(args.log, cutoff_ts))
    pairs = join_pairs(records)

    completed = [(oid, i, a) for oid, (i, a) in pairs.items()
                 if a is not None and a.get("outcome") == "complete"]
    if not completed:
        print(f"No completed trades in last {args.days} days. Log has "
              f"{len(records)} records, {len(pairs)} unique intents.")
        return 0

    ratios = []
    for oid, intent, actual in completed:
        r = compute_ratio(intent, actual)
        if r == r:  # not nan
            ratios.append((oid, r))

    if not ratios:
        print(f"No computable ratios across {len(completed)} completed trades.")
        return 0

    above = sum(1 for _, r in ratios if r >= args.min_ratio)
    pct = above / len(ratios)
    print(f"Fill quality over last {args.days} days:")
    print(f"  completed trades:       {len(completed)}")
    print(f"  computable ratios:      {len(ratios)}")
    print(f"  >= min_ratio ({args.min_ratio:.2f}): {above}/{len(ratios)} = {pct*100:.1f}%")
    print(f"  target:                 {args.target_pct*100:.0f}%")
    print(f"  worst 3:")
    for oid, r in sorted(ratios, key=lambda x: x[1])[:3]:
        print(f"    {oid}: {r:.3f}")

    if pct < args.target_pct:
        print(f"\n[ALERT] fill quality below target ({pct*100:.1f}% < {args.target_pct*100:.0f}%)")
        return 2
    print(f"\n[OK] fill quality meets target")
    return 0


if __name__ == "__main__":
    sys.exit(main())
