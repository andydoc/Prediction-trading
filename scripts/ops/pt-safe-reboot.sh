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
#   ALERT_AFTER_SECS — send Telegram alert if reboot keeps being deferred
#     for longer than this (default 86400 = 24h)
#   WORKSPACE — prediction-trader workspace (default /home/ubuntu/prediction-trader)

set -uo pipefail

log() { logger -t pt-safe-reboot -s -- "$*"; }

DASH_URL=${DASH_URL:-http://127.0.0.1:5558}
MAX_WAIT_SECS=${MAX_WAIT_SECS:-1800}
ALERT_AFTER_SECS=${ALERT_AFTER_SECS:-86400}
WORKSPACE=${WORKSPACE:-/home/ubuntu/prediction-trader}
STATE_DIR=/var/lib/prediction-trader
REASON_FILE=$STATE_DIR/start-reason
PENDING_SINCE_FILE=$STATE_DIR/safe-reboot-pending-since
ALERT_SENT_FILE=$STATE_DIR/safe-reboot-alert-sent

# Send a one-shot Telegram alert. Reads bot token from secrets.yaml and
# chat id from config.yaml via python3 (already a dependency for drain
# parsing). No-op if creds missing — don't block reboot flow on alerting.
send_telegram_alert() {
    local text="$1"
    local token chat
    token=$(python3 -c "import yaml,sys
try:
    print(yaml.safe_load(open('$WORKSPACE/config/secrets.yaml')).get('telegram_bot_token',''))
except Exception: pass" 2>/dev/null)
    chat=$(python3 -c "import yaml,sys
try:
    cfg=yaml.safe_load(open('$WORKSPACE/config/config.yaml'))
    # Chat id is nested under notifications/phone_number in some layouts
    for k in ('notifications','notify','telegram'):
        n=cfg.get(k) if isinstance(cfg, dict) else None
        if isinstance(n, dict) and n.get('phone_number'):
            print(n['phone_number']); sys.exit(0)
    # Top-level fallback
    if cfg.get('phone_number'): print(cfg['phone_number'])
except Exception: pass" 2>/dev/null)
    if [ -z "$token" ] || [ -z "$chat" ]; then
        log "telegram alert skipped: missing token or chat_id"
        return 1
    fi
    curl -fsS --max-time 10 -X POST \
        "https://api.telegram.org/bot${token}/sendMessage" \
        --data-urlencode "chat_id=${chat}" \
        --data-urlencode "text=${text}" \
        --data-urlencode "disable_web_page_preview=true" \
        >/dev/null 2>&1 && log "telegram alert sent" || log "telegram alert POST failed"
}

# Short-circuit: if no reboot is flagged, nothing to do. Clean up any
# stale pending/alert state from a previous cycle — a manual reboot by
# the operator would clear reboot-required without us noticing.
if [ ! -f /var/run/reboot-required ]; then
    rm -f "$PENDING_SINCE_FILE" "$ALERT_SENT_FILE"
    exit 0
fi

# Record when we first noticed reboot-required — used for the 24h alert.
# Stored in durable /var/lib so it survives even if the service unit is
# restarted between timer ticks.
mkdir -p "$STATE_DIR"
if [ ! -f "$PENDING_SINCE_FILE" ]; then
    date +%s > "$PENDING_SINCE_FILE"
fi
PENDING_SINCE=$(cat "$PENDING_SINCE_FILE" 2>/dev/null || echo "0")
PENDING_FOR=$(( $(date +%s) - PENDING_SINCE ))
log "reboot deferred for ${PENDING_FOR}s (threshold ${ALERT_AFTER_SECS}s)"

# One-shot 24h alert: if drain has been failing for >= ALERT_AFTER_SECS
# and we haven't alerted yet this cycle, ping Telegram. The flag file
# prevents re-alerting every 15 min — only one message per reboot cycle.
maybe_alert() {
    if [ -f "$ALERT_SENT_FILE" ]; then return; fi
    if [ "$PENDING_FOR" -lt "$ALERT_AFTER_SECS" ]; then return; fi
    local pkgs="(no pkg list)"
    [ -f /var/run/reboot-required.pkgs ] && \
        pkgs=$(tr '\n' ',' < /var/run/reboot-required.pkgs | sed 's/,$//')
    local hours=$((PENDING_FOR / 3600))
    send_telegram_alert "[SAFE-REBOOT] kernel/libc update deferred for ${hours}h — drain check keeps failing.
Host: $(hostname)
Packages: ${pkgs}
Check: journalctl -t pt-safe-reboot --since '1 day ago'
Force: sudo reboot (accepts mid-trade state)"
    touch "$ALERT_SENT_FILE"
}

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
        # If we've been deferring for >= 24h, send a one-shot Telegram alert so
        # the operator knows the update is stuck. No-op if we've already alerted
        # this cycle (PENDING_SINCE_FILE clears on successful reboot / when
        # reboot-required goes away).
        maybe_alert
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
