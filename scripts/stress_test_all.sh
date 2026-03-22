#!/usr/bin/env bash
# E2.6: Run stress tests for all 7 parameters serially.
# Usage: bash scripts/stress_test_all.sh [--cycle 3600] [--workspace /path]
#
# Writes combined results to data/stress_test.db.
# Each parameter's output is logged to logs/stress_<param>.log.

set -e

CYCLE="${1:-3600}"
WORKSPACE="${2:-/home/ubuntu/prediction-trader}"
PORT=5559
SETTLE=60
POLL=10

PARAMS=(
    max_evals_per_batch
    efp_drift_threshold
    efp_staleness_seconds
    constraint_rebuild_interval_seconds
    stale_sweep_interval_seconds
    stale_asset_threshold_seconds
    state_save_interval_seconds
)

cd "$WORKSPACE"
mkdir -p logs

echo "======================================"
echo " E2.6: Stress test all 7 parameters"
echo " Cycle: ${CYCLE}s  Settle: ${SETTLE}s"
echo " Workspace: ${WORKSPACE}"
echo " Estimated time: $(( ${#PARAMS[@]} * 5 * (CYCLE + SETTLE + 20) / 3600 )) hours"
echo "======================================"
echo ""

for param in "${PARAMS[@]}"; do
    echo "[$(date '+%H:%M:%S')] Starting: ${param}"
    logfile="logs/stress_${param}.log"

    python3 scripts/stress_test.py \
        --param "$param" \
        --cycle "$CYCLE" \
        --settle "$SETTLE" \
        --poll "$POLL" \
        --port "$PORT" \
        --workspace "$WORKSPACE" \
        2>&1 | tee "$logfile"

    echo "[$(date '+%H:%M:%S')] Completed: ${param}"
    echo ""
done

echo "======================================"
echo " All tests complete. Results in data/stress_test.db"
echo "======================================"
