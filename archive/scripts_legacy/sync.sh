#!/bin/bash
# sync.sh — Commit, push from WSL, pull to Windows clone
# Usage: ./sync.sh "commit message"
# Run from anywhere — uses absolute paths

MSG="${1:-Auto-sync}"
WSL_REPO="/home/andydoc/prediction-trader"
WIN_REPO="/mnt/c/Users/Andrew Thompson/Dev/Prediction-trader"

echo "=== WSL: commit & push ==="
cd "$WSL_REPO"
git add -A
git status --short
git commit -m "$MSG" 2>/dev/null && echo "Committed: $MSG" || echo "(nothing to commit)"
git push origin main

echo ""
echo "=== Windows: pull ==="
cd "$WIN_REPO"
git pull origin main

echo ""
echo "=== Done ==="
git log --oneline -3
