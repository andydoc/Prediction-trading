# scripts/ops — operational helpers for the Dublin VPS

Scripts and systemd units that live on the VPS filesystem (outside the repo
checkout) but are version-controlled here so they survive VPS rebuilds and
get a git history.

## Files

| File | Installed path on VPS | Purpose |
|------|------------------------|---------|
| `pt-service-wrapper.sh` | `/usr/local/bin/pt-service-wrapper.sh` | systemd `ExecStart` wrapper — reads one-shot `TRADER_START_REASON` from `/var/lib/prediction-trader/start-reason`, exports it, deletes file, execs binary. |
| `pt-safe-reboot.sh` | `/usr/local/bin/pt-safe-reboot.sh` | Drain-check + reboot. No-op unless `/var/run/reboot-required` exists. Waits (≤30 min) for live positions to close, eval queue to drain, WS to be healthy; then writes `start-reason=kernel_update_auto` and `systemctl reboot`. |
| `pt-safe-reboot.service` | `/etc/systemd/system/pt-safe-reboot.service` | Oneshot unit that runs the safe-reboot script. Gated by `ConditionPathExists=/var/run/reboot-required`. |
| `pt-safe-reboot.timer` | `/etc/systemd/system/pt-safe-reboot.timer` | Fires the safe-reboot service 5 min after boot, then every 15 min. |

## Startup reason tagging

The Rust binary reads `TRADER_START_REASON` env var on start and includes
its value in the Telegram `[STARTUP]` message. Default: `"manual"`.

Common reason values produced by these scripts:

| Reason | Source |
|--------|--------|
| `manual` | Default when no reason file exists (operator-initiated restart) |
| `kernel_update_auto` | Written by `pt-safe-reboot.sh` before the drain-gated reboot |
| `post_unattended_upgrade` | (Reserved) for future service-only restart after lib upgrades |

Set manually for one-off restarts via:
```bash
echo 'migration_test' | sudo tee /var/lib/prediction-trader/start-reason
sudo systemctl restart prediction-trader
```

## Install on a fresh VPS

```bash
sudo install -m 755 scripts/ops/pt-service-wrapper.sh /usr/local/bin/
sudo install -m 755 scripts/ops/pt-safe-reboot.sh     /usr/local/bin/
sudo install -m 644 scripts/ops/pt-safe-reboot.service /etc/systemd/system/
sudo install -m 644 scripts/ops/pt-safe-reboot.timer   /etc/systemd/system/
sudo mkdir -p /var/lib/prediction-trader

# Point the main service's ExecStart at the wrapper (once):
# Edit /etc/systemd/system/prediction-trader.service:
#   ExecStart=/usr/local/bin/pt-service-wrapper.sh
# (remove any --workspace arg — the wrapper handles it)

sudo systemctl daemon-reload
sudo systemctl enable --now pt-safe-reboot.timer
```

## Drain criteria

`pt-safe-reboot.sh` blocks reboot until all of:

- `open_positions` (live PM) = 0 — never reboot mid-trade
- `queue_urgent` = 0 — no pending time-sensitive evaluations
- `ws_live` ≥ 1 — not rebooting into a reconnection storm

Background queue depth is ignored (long-running constraint refresh is not
trade-critical). Shadow positions are persistent and intentionally not
checked — they survive reboots via SQLite.

If drain doesn't happen within `MAX_WAIT_SECS` (default 1800 = 30 min),
the script exits 1 and the timer retries 15 min later.
