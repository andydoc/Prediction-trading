#!/bin/bash
echo "=== /home/andydoc/prediction-trader/ structure ==="
find /home/andydoc/prediction-trader -maxdepth 2 \
  ! -path "*/prediction-trader-env/*" \
  ! -path "*/__pycache__/*" \
  ! -path "*/.git/*" \
  | sort | sed 's|/home/andydoc/prediction-trader/||' | head -120

echo ""
echo "=== .sh files in /home/andydoc/prediction-trader/ ==="
find /home/andydoc/prediction-trader -maxdepth 3 -name "*.sh" \
  ! -path "*/prediction-trader-env/*" | sort

echo ""
echo "=== __pycache__ dirs ==="
find /home/andydoc/prediction-trader -name "__pycache__" \
  ! -path "*/prediction-trader-env/*" | sort

echo ""
echo "=== Loose .py files at root ==="
ls /home/andydoc/prediction-trader/*.py 2>/dev/null

echo ""
echo "=== Size of key items ==="
du -sh /home/andydoc/prediction-trader/logs 2>/dev/null
du -sh /home/andydoc/prediction-trader/data 2>/dev/null
du -sh /home/andydoc/prediction-trader/prediction-trader-env 2>/dev/null
