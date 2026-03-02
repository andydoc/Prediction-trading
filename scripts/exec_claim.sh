#!/bin/bash
# exec_claim.sh — Claim or release execution control from any machine
# Usage:
#   ./exec_claim.sh claim              # Claim for this machine
#   ./exec_claim.sh claim laptop       # Claim for "laptop"
#   ./exec_claim.sh release            # Release (leader only)
#   ./exec_claim.sh release --force    # Force release (any machine)
#   ./exec_claim.sh status             # Check who holds the lock
#   ./exec_claim.sh heartbeat          # Extend TTL
#
# Set EXEC_CTRL_URL env var to override server address:
#   EXEC_CTRL_URL=http://192.168.1.50:5557 ./exec_claim.sh status

EXEC_CTRL_URL="${EXEC_CTRL_URL:-http://localhost:5557}"
MACHINE="${2:-$(hostname)}"
ACTION="${1:-status}"

case "$ACTION" in
    status)
        echo "Checking execution lock at $EXEC_CTRL_URL..."
        curl -s "$EXEC_CTRL_URL/status" | python3 -m json.tool
        ;;
    claim)
        echo "Claiming execution for '$MACHINE' at $EXEC_CTRL_URL..."
        curl -s -X POST "$EXEC_CTRL_URL/claim" \
            -H "Content-Type: application/json" \
            -d "{\"machine\":\"$MACHINE\",\"ttl_seconds\":300}" | python3 -m json.tool
        ;;
    release)
        if [ "$2" = "--force" ]; then
            echo "Force-releasing execution lock at $EXEC_CTRL_URL..."
            curl -s -X POST "$EXEC_CTRL_URL/release" \
                -H "Content-Type: application/json" \
                -d "{\"force\":true}" | python3 -m json.tool
        else
            echo "Releasing execution for '$MACHINE' at $EXEC_CTRL_URL..."
            curl -s -X POST "$EXEC_CTRL_URL/release" \
                -H "Content-Type: application/json" \
                -d "{\"machine\":\"$MACHINE\"}" | python3 -m json.tool
        fi
        ;;
    heartbeat)
        echo "Sending heartbeat for '$MACHINE'..."
        curl -s -X POST "$EXEC_CTRL_URL/heartbeat" \
            -H "Content-Type: application/json" \
            -d "{\"machine\":\"$MACHINE\",\"ttl_seconds\":300}" | python3 -m json.tool
        ;;
    health)
        curl -s "$EXEC_CTRL_URL/health" | python3 -m json.tool
        ;;
    *)
        echo "Usage: $0 {status|claim [machine]|release [--force]|heartbeat|health}"
        echo "  Set EXEC_CTRL_URL env var to point at remote server"
        echo "  Example: EXEC_CTRL_URL=http://192.168.1.50:5557 $0 claim laptop"
        exit 1
        ;;
esac
