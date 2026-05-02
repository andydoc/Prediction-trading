#!/usr/bin/env python3
"""
Polymarket V2 wallet onboarding (INC-021).

After Polymarket's V2 migration on 2026-04-28, the CLOB no longer trades
against USDC.e directly — orders settle against a wrapped collateral token
called "Polymarket USD" (pUSD) at 0xC011a7E12a19f7B1f670d46F03B03f3342E82DFB.
Polymarket's web UI handles the conversion automatically, but operators in
geoblocked jurisdictions can't reach it. This script does it from the VPS
using a DEX aggregator (KyberSwap) that routes through Uniswap V4 pools
without needing a UI.

Steps performed (each idempotent — re-running is safe):

  1. approve(USDC.e -> KyberSwap router, $SWAP_AMOUNT)
  2. KyberSwap aggregator-routed swap (USDC.e -> pUSD, ~0.5% slippage on $5)
  3. approve(pUSD -> V2 CTF Exchange, MAX) — for buys on standard markets
  4. approve(pUSD -> V2 NegRisk Exchange, MAX) — for buys on negRisk markets
  5. approve(pUSD -> NegRiskAdapter, MAX) — actual ERC20 spender on negRisk
  6. setApprovalForAll(CTF -> V2 CTF Exchange, true) — for sells
  7. setApprovalForAll(CTF -> V2 NegRisk Exchange, true) — for sells

Pre: wallet has USDC.e and POL gas.
Post: wallet has pUSD; V2 exchanges approved for MAX pUSD; CTF set up for sells.

Reads private key from /home/ubuntu/prediction-trader/config/secrets.yaml.
Does NOT log it.

Discovery + commit history is in INCIDENT_LOG.md → INC-021. The order of
operations above is what got Polymarket's CLOB to accept our first V2 order
(D2 probe at 2026-05-01 23:40 UTC, order_id 0x99270c39…6f21a).
"""

import json
import sys
import time
import urllib.request
import yaml
from web3 import Web3
from eth_account import Account

# --- Config ---
RPC = "https://1rpc.io/matic"
CHAIN_ID = 137

USDCE = Web3.to_checksum_address("0x2791bca1f2de4661ed88a30c99a7a9449aa84174")
PUSD = Web3.to_checksum_address("0xC011a7E12a19f7B1f670d46F03B03f3342E82DFB")
KYBER_ROUTER = Web3.to_checksum_address("0x6131B5fae19EA4f9D964eAc0408E4408b66337b5")
EXCHANGE_V2 = Web3.to_checksum_address("0xE111180000d2663C0091e4f400237545B87B996B")
NEG_RISK_EXCHANGE_V2 = Web3.to_checksum_address("0xe2222d279d744050d28e00520010520000310F59")
NEG_RISK_ADAPTER = Web3.to_checksum_address("0xd91E80cF2E7be2e162c6513ceD06f1dD0dA35296")
CONDITIONAL_TOKENS = Web3.to_checksum_address("0x4D97DCd97eC945f40cF65F87097ACe5EA0476045")

SWAP_AMOUNT = 5_000_000  # $5 USDC.e (6 decimals)
MAX_UINT256 = 2**256 - 1

ERC20_ABI = json.loads("""[
    {"name":"approve","type":"function","stateMutability":"nonpayable",
     "inputs":[{"name":"spender","type":"address"},{"name":"value","type":"uint256"}],
     "outputs":[{"name":"","type":"bool"}]},
    {"name":"allowance","type":"function","stateMutability":"view",
     "inputs":[{"name":"owner","type":"address"},{"name":"spender","type":"address"}],
     "outputs":[{"name":"","type":"uint256"}]},
    {"name":"balanceOf","type":"function","stateMutability":"view",
     "inputs":[{"name":"a","type":"address"}],
     "outputs":[{"name":"","type":"uint256"}]}
]""")

CTF_ABI = json.loads("""[
    {"name":"setApprovalForAll","type":"function","stateMutability":"nonpayable",
     "inputs":[{"name":"operator","type":"address"},{"name":"approved","type":"bool"}],
     "outputs":[]},
    {"name":"isApprovedForAll","type":"function","stateMutability":"view",
     "inputs":[{"name":"owner","type":"address"},{"name":"operator","type":"address"}],
     "outputs":[{"name":"","type":"bool"}]}
]""")


def http_get(url, timeout=30):
    req = urllib.request.Request(url, headers={"User-Agent": "wrap-pusd/1.0"})
    return json.loads(urllib.request.urlopen(req, timeout=timeout).read())


def http_post(url, body, timeout=30):
    req = urllib.request.Request(
        url,
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json", "User-Agent": "wrap-pusd/1.0"},
    )
    return json.loads(urllib.request.urlopen(req, timeout=timeout).read())


def load_private_key():
    with open("/home/ubuntu/prediction-trader/config/secrets.yaml") as f:
        s = yaml.safe_load(f)
    return s["polymarket"]["private_key"]


def send_tx(w3, account, tx, label):
    print(f"\n>> {label}")
    tx["nonce"] = w3.eth.get_transaction_count(account.address)
    tx["chainId"] = CHAIN_ID
    if "gasPrice" not in tx and "maxFeePerGas" not in tx:
        gas_price = w3.eth.gas_price
        tx["gasPrice"] = int(gas_price * 1.2)  # 20% bump for inclusion speed
    if "gas" not in tx:
        tx["gas"] = w3.eth.estimate_gas(tx)
    signed = account.sign_transaction(tx)
    raw = signed.raw_transaction if hasattr(signed, "raw_transaction") else signed.rawTransaction
    h = w3.eth.send_raw_transaction(raw)
    print(f"   sent: {h.hex()}")
    receipt = w3.eth.wait_for_transaction_receipt(h, timeout=180)
    print(f"   status: {receipt.status} (block {receipt.blockNumber}, gas {receipt.gasUsed})")
    if receipt.status != 1:
        raise RuntimeError(f"{label} reverted")
    return receipt


def main():
    print("=== INC-021 V2 onboarding: USDC.e -> pUSD + approvals ===")
    pk = load_private_key()
    account = Account.from_key(pk)
    w3 = Web3(Web3.HTTPProvider(RPC))
    # Polygon is PoA — inject ExtraDataToPOAMiddleware
    try:
        from web3.middleware import ExtraDataToPOAMiddleware
        w3.middleware_onion.inject(ExtraDataToPOAMiddleware, layer=0)
    except ImportError:
        # fallback for older web3.py
        from web3.middleware import geth_poa_middleware  # type: ignore
        w3.middleware_onion.inject(geth_poa_middleware, layer=0)
    print(f"Account: {account.address}")

    # Pre-check balances
    usdce = w3.eth.contract(address=USDCE, abi=ERC20_ABI)
    pusd = w3.eth.contract(address=PUSD, abi=ERC20_ABI)
    pol_bal = w3.eth.get_balance(account.address)
    usdce_bal = usdce.functions.balanceOf(account.address).call()
    pusd_bal_pre = pusd.functions.balanceOf(account.address).call()
    print(f"POL gas: {pol_bal/1e18:.4f}")
    print(f"USDC.e:  {usdce_bal/1e6:.2f}")
    print(f"pUSD:    {pusd_bal_pre/1e6:.4f} (pre-swap)")
    if usdce_bal < SWAP_AMOUNT:
        print(f"FATAL: insufficient USDC.e ({usdce_bal/1e6:.2f} < {SWAP_AMOUNT/1e6})")
        sys.exit(1)

    # === STEP 1: approve KyberSwap router to spend SWAP_AMOUNT USDC.e (idempotent) ===
    cur_allowance = usdce.functions.allowance(account.address, KYBER_ROUTER).call()
    if cur_allowance < SWAP_AMOUNT:
        tx = usdce.functions.approve(KYBER_ROUTER, SWAP_AMOUNT).build_transaction({"from": account.address})
        send_tx(w3, account, tx, "approve(USDC.e -> KyberSwap router, $5)")
    else:
        print(f"\nKyberSwap router already approved for ${cur_allowance/1e6:.2f} USDC.e — skipping approve")

    # === STEP 2: get swap calldata from KyberSwap ===
    print("\n>> KyberSwap: build swap tx")
    routes = http_get(
        f"https://aggregator-api.kyberswap.com/polygon/api/v1/routes"
        f"?tokenIn={USDCE}&tokenOut={PUSD}&amountIn={SWAP_AMOUNT}"
    )
    summary = routes["data"]["routeSummary"]
    print(f"   amountOut: {int(summary['amountOut'])/1e6:.4f} pUSD (gas est ${float(summary['gasUsd']):.4f})")

    build = http_post(
        "https://aggregator-api.kyberswap.com/polygon/api/v1/route/build",
        {
            "routeSummary": summary,
            "sender": account.address,
            "recipient": account.address,
            "slippageTolerance": 100,  # 1% (basis points; KyberSwap uses bps where 100 = 1%)
            "deadline": int(time.time()) + 600,
        },
    )
    swap_data = build["data"]
    print(f"   router: {swap_data['routerAddress']}")
    print(f"   value:  {swap_data.get('amountIn')} USDC.e")

    # === STEP 3: execute the swap ===
    swap_tx = {
        "from": account.address,
        "to": Web3.to_checksum_address(swap_data["routerAddress"]),
        "data": swap_data["data"],
        "value": 0,  # ERC-20 to ERC-20, no native value
    }
    send_tx(w3, account, swap_tx, "KyberSwap swap (USDC.e -> pUSD)")

    pusd_bal_post = pusd.functions.balanceOf(account.address).call()
    print(f"\npUSD post-swap: {pusd_bal_post/1e6:.4f} (delta: +{(pusd_bal_post-pusd_bal_pre)/1e6:.4f})")
    if pusd_bal_post <= pusd_bal_pre:
        print("FATAL: pUSD balance did not increase")
        sys.exit(1)

    # === STEP 4: approve V2 CTF Exchange (standard) for MAX pUSD ===
    cur = pusd.functions.allowance(account.address, EXCHANGE_V2).call()
    if cur < MAX_UINT256 // 2:
        tx = pusd.functions.approve(EXCHANGE_V2, MAX_UINT256).build_transaction({"from": account.address})
        send_tx(w3, account, tx, "approve(pUSD -> V2 CTF Exchange, MAX)")
    else:
        print(f"\nV2 CTF Exchange already approved for unlimited pUSD")

    # === STEP 5: approve V2 NegRisk Exchange for MAX pUSD ===
    cur = pusd.functions.allowance(account.address, NEG_RISK_EXCHANGE_V2).call()
    if cur < MAX_UINT256 // 2:
        tx = pusd.functions.approve(NEG_RISK_EXCHANGE_V2, MAX_UINT256).build_transaction({"from": account.address})
        send_tx(w3, account, tx, "approve(pUSD -> V2 NegRisk Exchange, MAX)")
    else:
        print(f"\nV2 NegRisk Exchange already approved for unlimited pUSD")

    # === STEP 5b: approve NegRiskAdapter for MAX pUSD ===
    # The NegRiskAdapter (0xd91E80…35296) is the ACTUAL ERC-20 spender on
    # negRisk markets, not the V2 NegRisk Exchange. Without this approval,
    # negRisk BUYS reject with: "allowance: 0, spender: 0xd91E80…". Both
    # this AND the V2 NegRisk Exchange approval are required.
    cur = pusd.functions.allowance(account.address, NEG_RISK_ADAPTER).call()
    if cur < MAX_UINT256 // 2:
        tx = pusd.functions.approve(NEG_RISK_ADAPTER, MAX_UINT256).build_transaction({"from": account.address})
        send_tx(w3, account, tx, "approve(pUSD -> NegRiskAdapter, MAX)")
    else:
        print(f"\nNegRiskAdapter already approved for unlimited pUSD")

    # === STEP 6: setApprovalForAll on Conditional Tokens for V2 spenders (for sells) ===
    # Three spenders need approval for SELLS to work:
    #  - V2 CTF Exchange (standard markets)
    #  - V2 NegRisk Exchange (per the SDK example, though in practice the
    #    Adapter is what actually moves CTF on negRisk)
    #  - NegRiskAdapter — REQUIRED for negRisk SELLs; surfaced when
    #    cleanup attempt hit `allowance: 0 spender: 0xd91E80…` on a SELL.
    ctf = w3.eth.contract(address=CONDITIONAL_TOKENS, abi=CTF_ABI)
    for exch_name, exch_addr in [
        ("V2 CTF Exchange", EXCHANGE_V2),
        ("V2 NegRisk Exchange", NEG_RISK_EXCHANGE_V2),
        ("NegRiskAdapter", NEG_RISK_ADAPTER),
    ]:
        if not ctf.functions.isApprovedForAll(account.address, exch_addr).call():
            tx = ctf.functions.setApprovalForAll(exch_addr, True).build_transaction({"from": account.address})
            send_tx(w3, account, tx, f"setApprovalForAll(CTF -> {exch_name}, true)")
        else:
            print(f"\nCTF already setApprovalForAll for {exch_name}")

    # === Final state ===
    print("\n=== FINAL ===")
    print(f"USDC.e: {usdce.functions.balanceOf(account.address).call()/1e6:.2f}")
    print(f"pUSD:   {pusd.functions.balanceOf(account.address).call()/1e6:.4f}")
    print(f"pUSD allowance to V2 CTF Exchange:    {pusd.functions.allowance(account.address, EXCHANGE_V2).call()}")
    print(f"pUSD allowance to V2 NegRisk Exchange: {pusd.functions.allowance(account.address, NEG_RISK_EXCHANGE_V2).call()}")
    print(f"pUSD allowance to NegRiskAdapter:      {pusd.functions.allowance(account.address, NEG_RISK_ADAPTER).call()}")
    print(f"CTF setApproval V2 CTF Exchange:       {ctf.functions.isApprovedForAll(account.address, EXCHANGE_V2).call()}")
    print(f"CTF setApproval V2 NegRisk:            {ctf.functions.isApprovedForAll(account.address, NEG_RISK_EXCHANGE_V2).call()}")
    print("\nDone. Re-run D2 probe to validate end-to-end V2 trade path.")


if __name__ == "__main__":
    main()
