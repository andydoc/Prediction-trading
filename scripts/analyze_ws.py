#!/usr/bin/env python3
"""Analyze WS User Channel investigation data."""
import json
from collections import defaultdict

with open("data/ws_investigation.json") as f:
    data = json.load(f)

our_order_ids = set(data["our_buy_order_ids"])
messages = data["messages"]

print(f"Total messages: {len(messages)}")
print(f"Our buy order IDs: {len(our_order_ids)}")
print()

# Separate by event type
trades = [m for m in messages if m.get("type") == "TRADE"]
orders = [m for m in messages if m.get("type") != "TRADE"]
print(f"Trade events: {len(trades)}")
print(f"Order events: {len(orders)}")
print()

# Group trades by status
by_status = defaultdict(list)
for t in trades:
    by_status[t.get("status", "?")].append(t)
for status, msgs in sorted(by_status.items()):
    print(f"  {status}: {len(msgs)} events")
print()

# Group trades by taker_order_id and trace lifecycle
by_taker = defaultdict(list)
for t in trades:
    tid = t.get("taker_order_id", "")
    by_taker[tid].append(t)

print("=== TRADE LIFECYCLE BY taker_order_id ===")
for taker_id, events in sorted(by_taker.items(), key=lambda x: x[1][0].get("timestamp", "")):
    is_ours = taker_id in our_order_ids
    statuses = [e["status"] for e in events]
    sizes = [e.get("size") for e in events]
    tx_hashes = list(set(e.get("transaction_hash", "") for e in events))
    ids = [e["id"] for e in events]
    print(f"  taker={taker_id[:25]}... ours={is_ours}")
    print(f"    lifecycle: {' -> '.join(statuses)}")
    print(f"    sizes: {sizes}")
    print(f"    event ids: {[i[:12] for i in ids]}")
    print(f"    tx_hashes: {[h[:25] for h in tx_hashes if h]}")
    print()

# Check: does id stay consistent across lifecycle?
print("=== ID CONSISTENCY CHECK ===")
for taker_id, events in by_taker.items():
    ids = set(e["id"] for e in events)
    statuses = [e["status"] for e in events]
    label = "CONSISTENT" if len(ids) == 1 else f"DIFFERS ({len(ids)} unique)"
    print(f"  taker={taker_id[:25]}... id {label} lifecycle={statuses}")
print()

# Check: which of our buy order IDs appear as taker_order_id?
print("=== OUR ORDER IDS vs taker_order_id ===")
all_taker_ids = set(t.get("taker_order_id", "") for t in trades)
matched = our_order_ids & all_taker_ids
unmatched = our_order_ids - all_taker_ids
print(f"  Matched: {len(matched)}/{len(our_order_ids)} of our order IDs found as taker_order_id")
if unmatched:
    print(f"  Unmatched: {len(unmatched)} order IDs NOT seen in any WS trade event")
    for u in list(unmatched)[:3]:
        print(f"    Missing: {u[:40]}...")
print()

# Check: transaction_hash grouping
print("=== TRANSACTION HASH GROUPING ===")
by_tx = defaultdict(list)
for t in trades:
    tx = t.get("transaction_hash", "")
    if tx:
        by_tx[tx].append(t)
for tx, events in sorted(by_tx.items(), key=lambda x: x[1][0].get("timestamp", "")):
    statuses = [e["status"] for e in events]
    takers = list(set(e.get("taker_order_id", "")[:20] for e in events))
    sizes = [e.get("size") for e in events]
    print(f"  tx={tx[:25]}... statuses={statuses} takers={takers} sizes={sizes}")
print()

# Show all unique fields across all messages
print("=== ALL FIELDS (trade events only) ===")
all_fields = set()
for t in trades:
    all_fields.update(t.keys())
for f in sorted(all_fields):
    values = [str(t.get(f, "")) for t in trades]
    unique = set(values)
    print(f"  {f}: {len(unique)} unique / {len(values)} total")

# Show order events in detail
if orders:
    print()
    print("=== ORDER EVENTS ===")
    for o in orders:
        print(json.dumps(o, indent=2)[:500])
