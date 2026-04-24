#!/bin/bash
# pt-safe-reboot.sh — drain-safe reboot for prediction-trader after
# unattended-upgrade flags /var/run/reboot-required.
#
# Typically triggered by the pt-safe-reboot.timer (every 15 min). Exits
# as a no-op when no reboot is needed. When a reboot IS needed, waits for
# a drain window (no open live positions, eval queue quiet, WS shards
# healthy) before issuing systemctl reboot. Tags the post-boot start as
# "kernel_update_auto" so the Telegram startup message is self-documenting.
#
# Location: /usr/local/bin/pt-safe-reboot.sh (chmod 755, runs as root
# via the systemd service unit). Reboots the box — this is intentional
# because libc/kernel upgrades require a full reboot to take effect.
#
# Exit codes:
#   0 = nothing to do, or reboot initiated
#   1 = drain check failed this round (retry next tick)
#   2 = configuration error (binary/dashboard not found)
#
# Environment overrides:
#   DASH_URL — dashboard URL (default http://127.0.0.1:5558)
#   MAX_WAIT_SECS — abort drain wait after this many seconds (default 1800)

set -uo pipefail

log() { logger -t pt-safe-reboot -s -- "$*"; }

DASH_URL=${DASH_URL:-http://127.0.0.1:5558}
MAX_WAIT_SECS=${MAX_WAIT_SECS:-1800}
REASON_FILE=/var/lib/prediction-trader/start-reason

# Short-circuit: if no reboot is flagged, nothing to do. This keeps the
# timer cheap (it fires every 15 min).
if [ ! -f /var/run/reboot-required ]; then
    exit 0
fi

# Report which packages triggered the requirement (visible in journalctl).
if [ -f /var/run/reboot-required.pkgs ]; then
    PKGS=$(tr '\n' ',' < /var/run/reboot-required.pkgs | sed 's/,$//')
    log "reboot-required by: $PKGS"
else
    log "reboot-required (no pkg list)"
fi

# Drain check. Re-run every 30s for up to MAX_WAIT_SECS. If the service
# is down we can reboot immediately — nothing to drain.
check_drained() {
    if ! systemctl is-active --quiet prediction-trader; then
        log "service not active — reboot immediately OK"
        return 0
    fi

    local state_json metrics_json
    state_json=$(curl -fsS --max-time 5 "${DASH_URL}/state" 2>/dev/null) || return 1
    metrics_json=$(curl -fsS --max-time 5 "${DASH_URL}/metrics" 2>/dev/null) || return 1

    # Python one-liner keeps the JSON parsing simple and robust. `python3`
    # is standard on Ubuntu. Extract the fields we care about:
    #   open = count of open LIVE positions (shadow state is durable, ignore)
    #   q_urgent = urgent queue depth
    #   q_bg = background queue depth
    #   ws_live = healthy WS connections
    #   ws_sub = subscribed asset count (proxy for "shards synced")
    local parsed
    parsed=$(python3 - <<PY 2>/dev/null
import json, sys
try:
    s = json.loads('''$state_json''')
    m = json.loads('''$metrics_json''')
    open_pos = len(s.get("open_positions", []))
    q_urg = int(m.get("queue_urgent", 0))
    q_bg = int(m.get("queue_background", 0))
    ws_live = int(m.get("ws_live", 0))
    ws_sub = int(m.get("ws_subscribed", 0))
    print(f"{open_pos} {q_urg} {q_bg} {ws_live} {ws_sub}")
except Exception as e:
    sys.exit(1)
PY
)
    [ -z "$parsed" ] && return 1
    read -r OPEN Q_URG Q_BG WS_LIVE WS_SUB <<< "$parsed"

    # Drain criteria (open positions are OK — see note below):
    #  - urgent eval queue empty (don't reboot mid-evaluation; background
    #    queue is allowed non-zero because it's long-running constraint
    #    refresh, not trading work)
    #  - at least one healthy WS connection (avoids rebooting during a
    #    reconnection storm when we'd restart in the same unhealthy state)
    #
    # Why open positions are NOT a blocker:
    #  - Resolved markets are detected on startup by check_api_resolutions
    #    (rust_engine/src/lib.rs) for both live (Data API) and shadow paths,
    #    and auto-close with correct P&L.
    #  - Partial fills that landed offline are reconciled via quantity
    #    mismatch detection in apply_reconciliation (same file).
    #  - If we gated on open positions the box could go weeks without
    #    rebooting, accumulating unpatched kernel/libc vulnerabilities.
    log "drain check: open=$OPEN q_urg=$Q_URG q_bg=$Q_BG ws_live=$WS_LIVE ws_sub=$WS_SUB"

    if [ "$Q_URG" -gt 0 ]; then
        log "not drained: $Q_URG urgent queue items (mid-evaluation)"
        return 1
    fi
    if [ "$WS_LIVE" -lt 1 ]; then
        log "not drained: no healthy WS connections ($WS_LIVE) — might be reconnecting"
        return 1
    fi

    return 0
}

START=$(date +%s)
while true; do
    if check_drained; then
        break
    fi
    NOW=$(date +%s)
    ELAPSED=$((NOW - START))
    if [ "$ELAPSED" -ge "$MAX_WAIT_SECS" ]; then
        log "drain wait exceeded ${MAX_WAIT_SECS}s — aborting this cycle, will retry on next timer tick"
        exit 1
    fi
    sleep 30
done

# Drained. Tag the next start so Telegram reports why we rebooted, then reboot.
mkdir -p "$(dirname "$REASON_FILE")"
echo "kernel_update_auto" > "$REASON_FILE"
chmod 644 "$REASON_FILE"

log "drained — writing start-reason and rebooting"
sync
systemctl reboot
