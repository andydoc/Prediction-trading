#!/bin/bash
# pt-service-wrapper.sh — systemd ExecStart wrapper for prediction-trader.
#
# Purpose: read a one-shot startup reason from a durable file (written by
# pt-safe-reboot.sh before reboot), export it as TRADER_START_REASON so the
# Rust binary can include it in the Telegram startup message, then delete
# the file so the next restart gets the default reason ("manual").
#
# Location: installed at /usr/local/bin/pt-service-wrapper.sh (chmod 755).
# Referenced by prediction-trader.service ExecStart.
#
# The file is at /var/lib/prediction-trader/start-reason — survives reboots
# (unlike /run/*) so kernel-update reboots correctly tag the post-boot start.

set -u  # don't set -e: we never want a missing reason file to block startup

REASON_FILE=/var/lib/prediction-trader/start-reason
WORKSPACE=${TRADER_WORKSPACE:-/home/ubuntu/prediction-trader}
BINARY=${TRADER_BINARY:-$WORKSPACE/target/release/prediction-trader}

if [ -f "$REASON_FILE" ]; then
    # Read reason, strip whitespace, limit length to avoid Telegram overflow.
    REASON=$(tr -d '\n\r' < "$REASON_FILE" | head -c 200)
    if [ -n "$REASON" ]; then
        export TRADER_START_REASON="$REASON"
    fi
    rm -f "$REASON_FILE"
fi

# If no reason was set from file, don't export anything — the Rust binary
# falls back to "manual" via unwrap_or_else.

exec "$BINARY" --workspace "$WORKSPACE" "$@"
