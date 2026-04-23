#!/usr/bin/env python3
"""Deep-merge live-d.yaml into config.yaml for go-live.

The --instance live-d overlay mechanism silently drops keys not in
ALLOWED_SET_KEYS (including live_trading.initial_capital, which is the
whole point of going live). So instead we merge the overlay into base
config in-place before service restart. Existing DB (data/state_rust.db)
is preserved — shadow portfolios keep compounding.

History: originally kept in C:\temp\pdfgen\ (unversioned). Moved into
scripts/ during the Dublin rebuild (2026-04-23) so it survives VPS/laptop
loss and gets synced via git pull.
"""
import sys, os, shutil, datetime
try:
    import yaml
except ImportError:
    print("ERROR: python3-yaml not installed. Run: sudo apt-get install -y python3-yaml", file=sys.stderr)
    sys.exit(1)

ROOT = os.path.expanduser("~/prediction-trader")
BASE = os.path.join(ROOT, "config/config.yaml")
OVERLAY = os.path.join(ROOT, "config/instances/live-d.yaml")
now = datetime.datetime.utcnow().strftime("%Y%m%d_%H%M%S")
BACKUP = BASE + f".pre_live_{now}"

def deep_merge(base, overlay):
    if isinstance(base, dict) and isinstance(overlay, dict):
        out = dict(base)
        for k, v in overlay.items():
            if k in out:
                out[k] = deep_merge(out[k], v)
            else:
                out[k] = v
        return out
    return overlay

with open(BASE, 'r') as f:
    base = yaml.safe_load(f)
with open(OVERLAY, 'r') as f:
    overlay = yaml.safe_load(f)

# Preserve state.db_path explicitly so we keep the shadow-history DB
preserved_db_path = base.get('state', {}).get('db_path', 'data/state_rust.db')

merged = deep_merge(base, overlay)

# Force db_path back (live-d overlay doesn't set it, but belt-and-braces)
if 'state' not in merged:
    merged['state'] = {}
merged['state']['db_path'] = preserved_db_path

# ---- Hard asserts: things that MUST be true for a valid live-d merge ----
assert merged['live_trading']['shadow_only'] is False, "shadow_only MUST be false for live mode"
assert merged['live_trading']['initial_capital'] == 100, "initial_capital MUST be 100"
assert merged['live_trading']['max_capital'] == 200, "max_capital MUST be 200"
assert merged['arbitrage']['capital_per_trade_pct'] == 0.30
assert merged['arbitrage']['max_concurrent_positions'] == 8
assert merged['arbitrage']['min_profit_threshold'] == 0.04
assert merged['safety']['circuit_breaker']['max_drawdown_pct'] == 0.35
assert merged['safety']['gas_monitor']['critical_pol_balance'] == 0.5
assert merged['safety']['usdc_monitor']['enabled'] is True
assert 'publicnode' in merged['safety']['usdc_monitor']['rpc_url'], \
    f"usdc_monitor.rpc_url must be publicnode (serde default polygon-rpc.com is IP-banned), got {merged['safety']['usdc_monitor']['rpc_url']!r}"
assert merged['dashboard']['port'] == 5558

# ---- Soft checks: warn instead of assert for operational flags that vary ----
#      (execute_orders flips during validation windows; hostname changes per VPS)
if merged['live_trading']['execute_orders'] is not True:
    print(f"  NOTE: execute_orders={merged['live_trading']['execute_orders']} (disarmed — shadow evals only)")
valid_hosts = {'madrid', 'dublin'}
if merged['notifications']['hostname'] not in valid_hosts:
    print(f"  WARNING: notifications.hostname={merged['notifications']['hostname']!r} not in {valid_hosts}")

# Backup base before writing
shutil.copy2(BASE, BACKUP)
print(f"Backed up base config -> {BACKUP}")

with open(BASE, 'w') as f:
    yaml.safe_dump(merged, f, default_flow_style=False, sort_keys=False, width=120)

print("Wrote merged config to", BASE)
print("Critical settings confirmed:")
print(f"  live_trading.shadow_only         = {merged['live_trading']['shadow_only']}")
print(f"  live_trading.execute_orders      = {merged['live_trading']['execute_orders']}")
print(f"  live_trading.initial_capital     = {merged['live_trading']['initial_capital']}")
print(f"  live_trading.max_capital         = {merged['live_trading']['max_capital']}")
print(f"  live_trading.max_positions       = {merged['live_trading']['max_positions']}")
print(f"  live_trading.low_balance_pause   = {merged['live_trading']['low_balance_pause_usd']}")
print(f"  arbitrage.capital_per_trade_pct  = {merged['arbitrage']['capital_per_trade_pct']}")
print(f"  arbitrage.max_concurrent_pos     = {merged['arbitrage']['max_concurrent_positions']}")
print(f"  arbitrage.min_profit_threshold   = {merged['arbitrage']['min_profit_threshold']}")
print(f"  arbitrage.max_position_size      = {merged['arbitrage']['max_position_size']}")
print(f"  arbitrage.max_exposure_per_mkt   = {merged['arbitrage']['max_exposure_per_market']}")
print(f"  safety.cb.max_drawdown_pct       = {merged['safety']['circuit_breaker']['max_drawdown_pct']}")
print(f"  safety.gas.critical_pol_balance  = {merged['safety']['gas_monitor']['critical_pol_balance']}")
print(f"  safety.usdc.enabled              = {merged['safety']['usdc_monitor']['enabled']}")
print(f"  safety.usdc.rpc_url              = {merged['safety']['usdc_monitor']['rpc_url']}")
print(f"  safety.usdc.check_interval_secs  = {merged['safety']['usdc_monitor']['check_interval_seconds']}")
print(f"  safety.usdc.warning_balance      = {merged['safety']['usdc_monitor']['warning_balance']}")
print(f"  safety.usdc.critical_balance     = {merged['safety']['usdc_monitor']['critical_balance']}")
print(f"  notifications.hostname           = {merged['notifications']['hostname']}")
print(f"  dashboard.port                   = {merged['dashboard']['port']}")
print(f"  state.db_path (preserved)        = {merged['state']['db_path']}")
