#!/bin/bash
# watch_trader.sh — Real-time trading activity monitor
# Works on both WSL and VPS:
#   WSL: ./scripts/watch_trader.sh
#   VPS: ./scripts/watch_trader.sh                    (auto-detects journald)
#   VPS: ./scripts/watch_trader.sh shadow-a            (specific instance)
#
# Shows: opportunities, entries, exits, resolutions, errors, book depth issues
# Color-coded for quick visual scanning

INSTANCE="${1:-}"

# Auto-detect: journald (VPS/systemd) vs log files (WSL)
if systemctl is-active --quiet "prediction-trader@${INSTANCE:-shadow-a}" 2>/dev/null; then
    MODE="journal"
    UNIT="prediction-trader@${INSTANCE:-shadow-a}"
    echo "=== Prediction Trader Monitor (journald: $UNIT) ==="
    echo "Press Ctrl+C to stop"
    echo "===================================================="
    echo ""
    SOURCE_CMD="journalctl -u $UNIT -f -n 50 --no-pager -o cat"
else
    MODE="file"
    # Detect workspace: VPS default vs WSL default
    if [ -d "$HOME/prediction-trader" ]; then
        WORKSPACE="${WORKSPACE:-$HOME/prediction-trader}"
    else
        WORKSPACE="${WORKSPACE:-/home/andydoc/prediction-trader}"
    fi
    LOG_DIR="$WORKSPACE/logs"

    # Find today's supervisor log
    TODAY=$(date -u +%Y-%m-%d)
    LOG_FILE="$LOG_DIR/supervisor.$TODAY"

    if [ ! -f "$LOG_FILE" ]; then
        echo "No log file for today: $LOG_FILE"
        echo "Available logs:"
        ls -lt "$LOG_DIR"/supervisor.* 2>/dev/null | head -5
        exit 1
    fi

    echo "=== Prediction Trader Monitor ==="
    echo "Watching: $LOG_FILE"
    echo "Press Ctrl+C to stop"
    echo "================================="
    echo ""
    SOURCE_CMD="tail -n 50 -f $LOG_FILE"
fi

# Tail the log, filtering to important events with color coding
$SOURCE_CMD | while IFS= read -r line; do
    # Entry events (green)
    if echo "$line" | grep -qE 'ENTER:|PositionEntry'; then
        echo -e "\033[32m$line\033[0m"
    # Resolution events (cyan)
    elif echo "$line" | grep -qE 'WS RESOLUTION:|API RESOLUTION:'; then
        echo -e "\033[36m$line\033[0m"
    # Proactive exit (yellow)
    elif echo "$line" | grep -qE 'PROACTIVE EXIT:|Sold:'; then
        echo -e "\033[33m$line\033[0m"
    # Replacement (magenta)
    elif echo "$line" | grep -qE 'REPLACE:|WITH:'; then
        echo -e "\033[35m$line\033[0m"
    # Depth/staleness skip (dim yellow)
    elif echo "$line" | grep -qE 'SKIP \(depth\)|SKIP \(stale|SKIP \(no book\)'; then
        echo -e "\033[2;33m$line\033[0m"
    # Errors (red bold)
    elif echo "$line" | grep -qiE 'ERROR|WARN|panic|failed'; then
        echo -e "\033[1;31m$line\033[0m"
    # Stats line (dim)
    elif echo "$line" | grep -qE '^\[iter|Capital='; then
        echo -e "\033[2m$line\033[0m"
    # WhatsApp notifications
    elif echo "$line" | grep -qE 'WhatsApp|notification sent'; then
        echo -e "\033[34m$line\033[0m"
    # State saved
    elif echo "$line" | grep -qE 'State saved:'; then
        echo -e "\033[2m$line\033[0m"
    # Constraint rebuild
    elif echo "$line" | grep -qE 'Constraints:|Scanner refresh'; then
        echo -e "\033[2;36m$line\033[0m"
    # AI validation
    elif echo "$line" | grep -qE 'SKIP \(unrepresented|SKIP \(AI date\)|Postponement'; then
        echo -e "\033[33m$line\033[0m"
    # Circuit breaker (red bold)
    elif echo "$line" | grep -qE 'CIRCUIT BREAKER|circuit_breaker'; then
        echo -e "\033[1;31;7m$line\033[0m"
    fi
done
