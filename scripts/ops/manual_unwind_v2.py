#!/usr/bin/env python3
"""
INC-D9 manual unwind helper (Polymarket V2).

When the bot detects a partial submit (multi-leg arb where the CLOB accepts
some legs and rejects others), the orchestrator now trips the circuit breaker
and refuses to enter further positions until the operator manually clears the
stranded inventory. This script is the operator's recovery tool.

Why a hand-rolled signer? py-clob-client v0.34.6 (latest pip) still hardcodes
the V1 exchange addresses in `py_clob_client/config.py`. Submitting a SELL via
that client returns `HTTP 400 order_version_mismatch`. We mirror the bot's own
V2 EIP-712 signing + L2 HMAC (rust_engine/src/signing.rs) so the unwind path
matches what the live executor would emit if it were wired.

V2 schema reference (signing.rs:54, 130-146, 156-177):
- domain: name="Polymarket CTF Exchange", version="2", chainId=137,
          verifyingContract = NEG_RISK_EXCHANGE_V2 (0xe222...0F59) for negRisk,
          CTF_EXCHANGE_V2  (0xE111...996B) for standard markets.
- struct: 11 fields in order — salt, maker, signer, tokenId, makerAmount,
  takerAmount, side, signatureType, timestamp, metadata, builder.
- signatureType = 0 (EOA) per V2 SDK signature_type_v2.py. The doc-comment in
  signing.rs that says "V2 EOA = 1" is stale; the real code uses 0.
- side: BUY=0, SELL=1 (signed); "BUY"/"SELL" string on the wire.
- amounts in raw 6-decimal units (CTF and pUSD both 6 decimals).
- For FAK SELL: makerAmount = shares * 1e6, takerAmount = shares * price * 1e6.

L2 HMAC (signing.rs:493-523):
- secret = base64.urlsafe_b64decode(clob_api_secret)
- message = ts + "POST" + "/order" + body_str
- POLY_SIGNATURE = base64.urlsafe_b64encode(HMAC-SHA256(secret, message))

Usage:
  # Discover what's stranded:
  python3 manual_unwind_v2.py --list

  # Unwind a specific token:
  python3 manual_unwind_v2.py --token-id <decimal> --shares <float> [--neg-risk]

  # Auto-unwind everything held (interactive confirmation per token):
  python3 manual_unwind_v2.py --auto

After running, restart prediction-trader to clear the partial-fill CB trip.

First successful run: 2026-05-07 09:08 UTC, sold 2.57 shares of "Germany vs
Curaçao Draw NO" at $0.92 (entry $0.86, profit +$0.16). Tx
0xdae306811b4287f00ccda07514c2a3bf195aee73656f70a01c52225561808365.
"""
import argparse
import base64
import hashlib
import hmac
import json
import secrets as pysecrets
import sys
import time
import urllib.request

import requests
import yaml
from eth_account import Account
from eth_account.messages import encode_typed_data

# --- Constants ---------------------------------------------------------------

DEFAULT_SECRETS = "/home/ubuntu/prediction-trader/config/secrets.yaml"
CLOB_HOST = "https://clob.polymarket.com"
DATA_API = "https://data-api.polymarket.com"
GAMMA_API = "https://gamma-api.polymarket.com"
CHAIN_ID = 137

# V2 contracts (mirrors signing.rs:30-33)
CTF_EXCHANGE_V2 = "0xE111180000d2663C0091e4f400237545B87B996B"
NEG_RISK_EXCHANGE_V2 = "0xe2222d279d744050d28e00520010520000310F59"

ZERO_BYTES32 = "0x" + "00" * 32
USER_AGENT = "Mozilla/5.0"  # Data API and Gamma both Cloudflare-block default UAs


# --- Helpers ----------------------------------------------------------------

def load_secrets(path):
    with open(path) as f:
        sec = yaml.safe_load(f)["polymarket"]
    return sec


def http_get_json(url, timeout=15):
    req = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    return json.loads(urllib.request.urlopen(req, timeout=timeout).read().decode())


def list_holdings(addr):
    """Query Polymarket Data API for all current positions held by address.

    Returns a list of dicts: {asset, conditionId, size, avgPrice, title, negRisk}.
    Filters out zero-size positions.
    """
    url = f"{DATA_API}/positions?user={addr.lower()}&sizeThreshold=0"
    data = http_get_json(url)
    holdings = []
    for p in data:
        size = float(p.get("size", 0) or 0)
        if size <= 0:
            continue
        # Data API doesn't always include negRisk flag — fall back to gamma lookup
        neg_risk = p.get("negativeRisk", False)
        holdings.append({
            "asset": p.get("asset", ""),
            "conditionId": p.get("conditionId", ""),
            "size": size,
            "avgPrice": float(p.get("avgPrice", 0) or 0),
            "title": p.get("title", "")[:60],
            "outcome": p.get("outcome", "?"),
            "negRisk": neg_risk,
        })
    return holdings


def market_neg_risk(token_id):
    """Look up whether a token belongs to a negRisk market via Gamma."""
    # Gamma doesn't have a "find market by token_id" endpoint — caller should
    # pass --neg-risk explicitly if they know. Default to True for sports
    # (most common partial-fill case) is a safer bet for unwinds.
    return True


def fetch_best_bid(token_id):
    """Return (best_bid_price, top_of_book_size) or raise."""
    r = requests.get(f"{CLOB_HOST}/book", params={"token_id": token_id}, timeout=10)
    r.raise_for_status()
    book = r.json()
    bids = book.get("bids", [])
    if not bids:
        raise RuntimeError(
            f"no bids on token {token_id[:18]}... — book is one-sided, cannot unwind via market sell."
        )
    best = max(float(b["price"]) for b in bids)
    top_size = sum(float(b["size"]) for b in bids if abs(float(b["price"]) - best) < 1e-9)
    return best, top_size


def submit_v2_sell(secrets, token_id, shares, price, neg_risk, dry_run=False):
    """Sign + POST a V2 FAK SELL order. Returns the parsed JSON response."""
    priv_key = secrets["private_key"]
    acct = Account.from_key(priv_key)
    maker = acct.address

    # Quantize shares to 2 decimals (FAK amount_decimals default).
    shares_q = int(shares * 100) / 100
    if shares_q <= 0:
        raise ValueError(f"shares quantized to 0 (input={shares}); nothing to sell.")

    maker_amount = int(round(shares_q * 1_000_000))         # CTF tokens, 6 dec
    taker_amount = int(round(shares_q * price * 1_000_000))  # pUSD, 6 dec

    salt = pysecrets.randbits(53)
    ts_ms = int(time.time() * 1000)
    verifying = NEG_RISK_EXCHANGE_V2 if neg_risk else CTF_EXCHANGE_V2

    typed_data = {
        "types": {
            "EIP712Domain": [
                {"name": "name", "type": "string"},
                {"name": "version", "type": "string"},
                {"name": "chainId", "type": "uint256"},
                {"name": "verifyingContract", "type": "address"},
            ],
            "Order": [
                {"name": "salt", "type": "uint256"},
                {"name": "maker", "type": "address"},
                {"name": "signer", "type": "address"},
                {"name": "tokenId", "type": "uint256"},
                {"name": "makerAmount", "type": "uint256"},
                {"name": "takerAmount", "type": "uint256"},
                {"name": "side", "type": "uint8"},
                {"name": "signatureType", "type": "uint8"},
                {"name": "timestamp", "type": "uint256"},
                {"name": "metadata", "type": "bytes32"},
                {"name": "builder", "type": "bytes32"},
            ],
        },
        "primaryType": "Order",
        "domain": {
            "name": "Polymarket CTF Exchange",
            "version": "2",
            "chainId": CHAIN_ID,
            "verifyingContract": verifying,
        },
        "message": {
            "salt": salt,
            "maker": maker,
            "signer": maker,
            "tokenId": int(token_id),
            "makerAmount": maker_amount,
            "takerAmount": taker_amount,
            "side": 1,                   # SELL
            "signatureType": 0,          # V2 EOA
            "timestamp": ts_ms,
            "metadata": ZERO_BYTES32,
            "builder": ZERO_BYTES32,
        },
    }

    signable = encode_typed_data(full_message=typed_data)
    signed_msg = acct.sign_message(signable)
    sig_hex = "0x" + signed_msg.signature.hex()

    order_body = {
        "order": {
            "salt": salt,                          # JSON number
            "maker": maker,
            "signer": maker,
            "tokenId": token_id,
            "makerAmount": str(maker_amount),
            "takerAmount": str(taker_amount),
            "side": "SELL",
            "expiration": "0",
            "signatureType": 0,
            "timestamp": str(ts_ms),
            "metadata": ZERO_BYTES32,
            "builder": ZERO_BYTES32,
            "signature": sig_hex,
        },
        "owner": secrets["clob_api_key"],
        "orderType": "FAK",
        "deferExec": False,
        "postOnly": False,
    }
    body_str = json.dumps(order_body, separators=(",", ":"))

    print(f"  >> SELL plan: shares={shares_q} @ {price}")
    print(f"     makerAmount={maker_amount}, takerAmount={taker_amount}")
    print(f"     verifyingContract={verifying}")
    print(f"     order_hash={signed_msg.message_hash.hex()[:24]}...")

    if dry_run:
        print("  >> DRY RUN — not posting")
        return {"dry_run": True, "order_body": order_body}

    # L2 HMAC headers
    ts_s = str(int(time.time()))
    secret_bytes = base64.urlsafe_b64decode(secrets["clob_api_secret"])
    hmac_msg = ts_s + "POST" + "/order" + body_str
    mac = hmac.new(secret_bytes, hmac_msg.encode(), hashlib.sha256)
    poly_sig = base64.urlsafe_b64encode(mac.digest()).decode()

    headers = {
        "POLY_ADDRESS": maker,
        "POLY_SIGNATURE": poly_sig,
        "POLY_TIMESTAMP": ts_s,
        "POLY_API_KEY": secrets["clob_api_key"],
        "POLY_PASSPHRASE": secrets["clob_passphrase"],
        "Content-Type": "application/json",
    }
    resp = requests.post(f"{CLOB_HOST}/order", data=body_str, headers=headers, timeout=15)
    return {"status": resp.status_code, "body": resp.text}


# --- Main --------------------------------------------------------------------

def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--secrets", default=DEFAULT_SECRETS,
                    help=f"Path to secrets.yaml (default: {DEFAULT_SECRETS})")
    ap.add_argument("--list", action="store_true",
                    help="List all current holdings via Data API and exit.")
    ap.add_argument("--token-id", help="Decimal token ID to sell.")
    ap.add_argument("--shares", type=float, help="Number of shares to sell.")
    ap.add_argument("--price", type=float,
                    help="Override sell price (default: use top-of-book bid).")
    ap.add_argument("--neg-risk", action="store_true", default=False,
                    help="Token belongs to a negRisk market (sports). Default: standard CTF.")
    ap.add_argument("--auto", action="store_true",
                    help="Discover holdings via Data API and unwind each (with confirmation).")
    ap.add_argument("--dry-run", action="store_true",
                    help="Build + sign the order but don't POST it.")
    ap.add_argument("--yes", action="store_true",
                    help="Skip interactive confirmation in --auto mode.")
    args = ap.parse_args()

    secrets = load_secrets(args.secrets)
    addr = Account.from_key(secrets["private_key"]).address
    print(f"wallet: {addr}")

    if args.list or args.auto:
        print("\n>> Discovering holdings via Data API...")
        try:
            holdings = list_holdings(addr)
        except Exception as e:
            print(f"ERROR: Data API list failed: {e}", file=sys.stderr)
            sys.exit(2)
        if not holdings:
            print("   no holdings found.")
            return
        for i, h in enumerate(holdings):
            print(f"   [{i}] {h['title']}")
            print(f"       outcome={h['outcome']} size={h['size']} avg=${h['avgPrice']:.4f} "
                  f"asset={h['asset'][:18]}... negRisk={h['negRisk']}")

        if args.list:
            return
        # --auto: confirm each
        for h in holdings:
            print(f"\n>> Unwind candidate: {h['title']} ({h['outcome']}, {h['size']} shares)")
            try:
                best, top = fetch_best_bid(h["asset"])
            except Exception as e:
                print(f"   skip: {e}")
                continue
            print(f"   top-of-book bid: {best} (size {top})")
            price = args.price if args.price else best
            if not args.yes:
                ans = input(f"   sell {h['size']} shares at {price}? [y/N] ").strip().lower()
                if ans != "y":
                    print("   skipped by operator")
                    continue
            resp = submit_v2_sell(secrets, h["asset"], h["size"], price,
                                   neg_risk=h["negRisk"], dry_run=args.dry_run)
            print(f"   response: {resp}")
        return

    # Single-token unwind path
    if not args.token_id or args.shares is None:
        ap.error("--token-id and --shares are required (or use --auto / --list).")

    print(f"\n>> Fetching book for token {args.token_id[:18]}...")
    best, top = fetch_best_bid(args.token_id)
    print(f"   top-of-book bid: {best} (size {top})")
    price = args.price if args.price else best

    resp = submit_v2_sell(secrets, args.token_id, args.shares, price,
                           neg_risk=args.neg_risk, dry_run=args.dry_run)
    print(f"\nresponse: {json.dumps(resp, indent=2) if isinstance(resp, dict) else resp}")


if __name__ == "__main__":
    main()
