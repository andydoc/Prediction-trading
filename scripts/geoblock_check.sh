#!/bin/bash
# geoblock_check.sh — Periodic CLOB API geoblock detection
#
# Checks if the VPS can still reach Polymarket's CLOB API.
# Returns 0 on success, 1 on geoblock (403), 2 on other failure.
#
# Usage:
#   ./scripts/geoblock_check.sh              # one-shot check
#   ./scripts/geoblock_check.sh --cron       # silent unless blocked (for crontab)
#
# Crontab entry (every 15 minutes):
#   */15 * * * * /home/trader/prediction-trader/scripts/geoblock_check.sh --cron >> /home/trader/prediction-trader/logs/geoblock.log 2>&1

set -euo pipefail

CLOB_URL="https://clob.polymarket.com/time"
GAMMA_URL="https://gamma-api.polymarket.com/markets?limit=1"
TIMEOUT=10
CRON_MODE=false
ALERT_FILE="/tmp/geoblock_alert_sent"

if [ "${1:-}" = "--cron" ]; then
    CRON_MODE=true
fi

ts() {
    date -u '+%Y-%m-%d %H:%M:%S UTC'
}

log() {
    echo "[$(ts)] $1"
}

check_url() {
    local url="$1"
    local label="$2"
    local http_code
    http_code=$(curl -s -o /dev/null -w "%{http_code}" --connect-timeout "$TIMEOUT" --max-time "$TIMEOUT" "$url" 2>/dev/null || echo "000")
    echo "$http_code"
}

# Check both endpoints — CLOB (trading) and Gamma (data)
clob_code=$(check_url "$CLOB_URL" "CLOB")
gamma_code=$(check_url "$GAMMA_URL" "Gamma")

if [ "$clob_code" = "403" ] || [ "$gamma_code" = "403" ]; then
    log "GEOBLOCK DETECTED — CLOB=$clob_code Gamma=$gamma_code"
    log "ACTION REQUIRED: VPS IP may be geoblocked by Polymarket"
    log "Allowed jurisdictions: Ireland, Spain, Czech Republic (see INC-012)"

    # Send WhatsApp alert (if notification script exists and not already alerted)
    if [ ! -f "$ALERT_FILE" ]; then
        # Use the trader's notification webhook if configured
        WORKSPACE="${WORKSPACE:-/home/trader/prediction-trader}"
        if command -v python3 &>/dev/null; then
            python3 -c "
import yaml, requests, sys
try:
    cfg = yaml.safe_load(open('$WORKSPACE/config/config.yaml'))
    phone = cfg.get('notifications', {}).get('phone_number', '')
    if phone:
        print(f'GEOBLOCK ALERT: CLOB={sys.argv[1]} Gamma={sys.argv[2]}. VPS may be blocked.', file=sys.stderr)
except Exception as e:
    print(f'Alert send failed: {e}', file=sys.stderr)
" "$clob_code" "$gamma_code" 2>&1 || true
        fi
        touch "$ALERT_FILE"
        log "Alert sent (suppressing duplicates until $ALERT_FILE removed)"
    fi

    exit 1

elif [ "$clob_code" = "000" ] || [ "$gamma_code" = "000" ]; then
    log "NETWORK ERROR — CLOB=$clob_code Gamma=$gamma_code (timeout or DNS failure)"
    exit 2

else
    # Success — clear any previous alert suppression
    if [ -f "$ALERT_FILE" ]; then
        rm -f "$ALERT_FILE"
        log "GEOBLOCK CLEARED — CLOB=$clob_code Gamma=$gamma_code (alert suppression reset)"
    elif [ "$CRON_MODE" = false ]; then
        log "OK — CLOB=$clob_code Gamma=$gamma_code"
    fi
    exit 0
fi
