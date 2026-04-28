# Geoblock Runbook (F-pre-6)

Operational procedure for handling a geoblock on the live VPS. Backfills the
gate originally deferred to Dublin migration in v0.19.0.

**Reference incidents**: INC-012 (Frankfurt VPS 403'd 2026-03-17), VPS migration
ladder Frankfurt → Madrid → Dublin (2026-04-04 → 2026-04-23).

---

## What "geoblock" means here

Polymarket's CLOB and Gamma API enforce jurisdictional restrictions at the
edge: requests from disallowed regions return HTTP 403 with `X-Block-Reason: REGION`.
A geoblock can manifest at three layers:

| Layer | Symptom | Detection |
|------|---------|-----------|
| TCP/edge | Timeouts on `clob.polymarket.com` | Repeated WS reconnect failures |
| HTTP | 403 on every request | `journalctl -u prediction-trader \| grep "403"` |
| Order submission | Entries reject with `geoblock` code | Telegram alert + `evaluated_opportunities.rejected_reason='geoblock'` (post-INC-019 telemetry) |

Block can be triggered by: VPS provider IP range flagged, country added to
the deny list, or a CDN-side change. Frankfurt was flagged for entire region
(INC-012); Madrid was hosting-provider terminated (force-majeure migration).

---

## Allowed jurisdictions (verified 2026-04-23)

**Confirmed working**: Ireland (Dublin), Spain (Madrid).
**Confirmed blocked**: Germany (Frankfurt — INC-012).
**Untested but plausibly OK**: Czech Republic, Netherlands, France.
**Avoid**: any US state, UK, Australia, Canada (per Polymarket TOS).

When provisioning a replacement, prefer Ireland (Dublin) — that's our verified
baseline.

---

## Detection

### Automated (already running)

- `scripts/geoblock_check.sh` runs from cron every 15 minutes.
- Hits `https://clob.polymarket.com/health` and `https://gamma-api.polymarket.com/markets?limit=1`.
- On any 403, sends a Telegram alert. Dedup via `/tmp/geoblock_alert_sent`
  so the operator only gets one alert per cycle.
- Cron entry expected at `/etc/cron.d/pt-geoblock` (verify on each new VPS).

### Manual

```bash
# From the VPS
curl -sS -o /dev/null -w "%{http_code}\n" https://clob.polymarket.com/health
curl -sS -o /dev/null -w "%{http_code}\n" https://gamma-api.polymarket.com/markets?limit=1

# 200 = OK; 403 = blocked; timeout = either edge issue or fully blocked
```

If only one of CLOB / Gamma is blocked, that's a partial geoblock — log and
treat as full blocked, since live trading needs both.

### Telegram-side

Alerts are tagged `[hostname]` so you can tell which VPS is affected. If you
see `[dublin] GEOBLOCK detected on clob.polymarket.com (403 REGION)`, start
the migration procedure below.

---

## Capital exposure window

While migration is in progress, **open positions remain on the wallet** —
they're ERC-1155 tokens on Polygon. The geoblock affects only:

- New entries (bot can't submit)
- Proactive exits and unwinds (bot can't submit, INC-020 also relevant)
- Reconciliation (bot can't query CLOB for fills/state)

Resolution still happens: when a market settles, the platform pays out
on-chain regardless of any client geoblock. So a closed position will still
credit USDC to the wallet even if the bot is offline.

**Rule of thumb**: any open position can sit safely up to its resolution
deadline. If migration takes 4 hours and a position resolves in 2, you'll
miss the proactive-exit window but the resolution payout is unaffected.

For positions resolving within the migration window: accept holding to
resolution. Don't try heroics like submitting orders manually from a non-blocked
machine — the kill switch is there for a reason.

---

## Migration procedure

### Pre-requisites (one-time, before any incident)

1. **Spare provider account** with capacity in an allowed region (ZAP for
   Madrid, IS*hosting for Dublin currently). Pre-pay credit on file.
2. **Local SSH key** added to provider's web console for instant root access.
3. **State backup script** runs nightly: `scripts/backup_state.sh` rsyncs
   `/home/ubuntu/prediction-trader/data/` to a non-VPS location (laptop or
   second VPS). Verify it has run within 24h.

### When the alert fires

#### 1. Confirm and pause (5 min)

```bash
ssh dublin                              # or whichever VPS is flagged
curl -sS -o /dev/null -w "%{http_code}" https://clob.polymarket.com/health
# If 403, confirm geoblock (not just transient).
# If timeout, retry from a second network — could be edge issue.
```

If confirmed blocked:

```bash
# Trigger kill switch via dashboard or:
echo > /home/ubuntu/prediction-trader/data/kill_switch.flag
# Wait 5s for orchestrator to pick it up + cancel any open CLOB orders.
sudo systemctl stop prediction-trader
```

This prevents the bot from spamming 403s and accumulating retry backoff state.

#### 2. Snapshot current state (5 min)

```bash
ssh dublin-ubuntu 'cd /home/ubuntu/prediction-trader && \
  TS=$(date -u +%Y%m%d_%H%M%SZ) && \
  cp data/state_rust.db data/state_rust.db.geoblock_${TS} && \
  ls -la data/state_rust.db.geoblock_*'
```

Pull the snapshot to your laptop:

```bash
scp dublin-ubuntu:/home/ubuntu/prediction-trader/data/state_rust.db.geoblock_* ./
```

#### 3. Provision replacement VPS (30-60 min)

Prefer Ireland (Dublin) again — different IP range may not be blocked.
If multiple Dublin IPs in same provider get 403'd, switch provider.

Required spec: 4 vCPU, 8 GB RAM, 50 GB SSD, Ubuntu 22.04 or 24.04.

```bash
# On the new VPS as root:
adduser ubuntu
usermod -aG sudo ubuntu
# Copy SSH key from laptop:
ssh-copy-id ubuntu@<new-vps-ip>

# As ubuntu:
sudo apt update && sudo apt install -y build-essential pkg-config libssl-dev sqlite3 git curl jq
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source ~/.cargo/env
git clone https://github.com/andydoc/Prediction-trading.git prediction-trader
cd prediction-trader

# Restore state from your laptop:
scp ~/state_rust.db.geoblock_* ubuntu@<new-vps-ip>:/home/ubuntu/prediction-trader/data/state_rust.db
# Also: secrets.yaml (NOT in git) — copy from old VPS or password manager
```

#### 4. Verify new VPS is not also blocked (5 min)

```bash
curl -sS -o /dev/null -w "%{http_code}\n" https://clob.polymarket.com/health
curl -sS -o /dev/null -w "%{http_code}\n" https://gamma-api.polymarket.com/markets?limit=1
```

Both should return 200. If either is 403, **abort** — provision a different
provider/region. Do not start the engine until both pass.

#### 5. Build + install ops scripts (10 min)

```bash
cd ~/prediction-trader
cargo build --release -p prediction-trader

# Install ops infrastructure (per scripts/ops/README.md):
sudo install -m 755 scripts/ops/pt-service-wrapper.sh /usr/local/bin/
sudo install -m 755 scripts/ops/pt-safe-reboot.sh /usr/local/bin/
sudo install -m 644 scripts/ops/pt-safe-reboot.service /etc/systemd/system/
sudo install -m 644 scripts/ops/pt-safe-reboot.timer /etc/systemd/system/
sudo mkdir -p /var/lib/prediction-trader
sudo chown ubuntu:ubuntu /var/lib/prediction-trader

# Install main service unit (copy from old VPS or recreate per OPS_RUNBOOK.md)
# ExecStart must be /usr/local/bin/pt-service-wrapper.sh

sudo systemctl daemon-reload
sudo systemctl enable --now pt-safe-reboot.timer
```

Add geoblock cron entry:

```bash
echo "*/15 * * * * ubuntu /home/ubuntu/prediction-trader/scripts/geoblock_check.sh" | sudo tee /etc/cron.d/pt-geoblock
```

#### 6. Smoke-test in shadow mode (15 min)

Edit `config/config.yaml`: set `live_trading.shadow_only: true`. This puts
the engine in shadow-only mode for verification without risking the wallet.

```bash
echo "post_geoblock_migration_shadow_smoketest" > /var/lib/prediction-trader/start-reason
sudo systemctl start prediction-trader
sleep 30
sudo journalctl -u prediction-trader --no-pager | grep -E "STARTUP|reconcile|WS"
```

Watch for:
- `[STARTUP] Engine started` Telegram message with new hostname tag
- `B4.1 startup reconciliation` shows known-good positions match wallet
- `B:0..15 subscribed` for all WS shards

#### 7. Switch to live (5 min)

If shadow smoketest is clean for 5+ minutes:

```bash
# Edit config: shadow_only: false
echo "post_geoblock_migration_live" > /var/lib/prediction-trader/start-reason
sudo systemctl restart prediction-trader
```

Send a manual `[hostname] [GEOBLOCK_RECOVERY] live on <new_host>` Telegram
alert so the operator knows the migration is complete.

#### 8. Update DNS / SSH config / monitoring

- Update local `~/.ssh/config` `Host` entries.
- Update Dublin tunnel scheduled task target host.
- Update any monitoring dashboards that hardcode IPs.
- Update `MEMORY.md` and `vps_environment.md` with new IP.
- Old VPS: spin down ONLY after 7 days of clean operation on the new VPS,
  to allow rollback if needed.

---

## Test

The migration should be drilled at least once per quarter against a throwaway
provider account. Simulate by:

1. Spin up a second VPS in an allowed region.
2. Pretend the current VPS is geoblocked.
3. Run procedure end-to-end on shadow_only mode.
4. Time it. Target: <90 minutes from alert to live on new VPS.

Last drill: **never** — file as a Milestone G work item.

---

## What NOT to do

- **Don't try to bypass the geoblock with a VPN/proxy on the VPS itself.**
  Polymarket explicitly forbids this. Detection means permanent ban + funds
  forfeit.
- **Don't manually submit orders from your laptop or another non-blocked
  machine.** The bot's accounting will diverge from the wallet. Reconciliation
  on next bot start would flag this as Critical.
- **Don't remove `kill_switch.flag` until the new VPS is verified.** The bot
  retains all open positions; freeing it before migration is verified causes
  it to spam 403s on the blocked VPS.

---

## Cross-references

- INC-012 (Frankfurt 403): `INCIDENT_LOG.md`
- VPS environment: `vps_environment.md` (memory)
- Backup script: `scripts/backup_state.sh`
- Geoblock check: `scripts/geoblock_check.sh`
- Ops scripts: `scripts/ops/README.md`
