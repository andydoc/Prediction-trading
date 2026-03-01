#!/bin/bash
cd /home/andydoc/prediction-trader
source /home/andydoc/prediction-trader-env/bin/activate
rm -f *.pid
nohup python main.py > logs/main.log 2>&1 &
echo "Started PID=$!"
sleep 8
echo "=== Processes ==="
ps aux | grep -E "main\.py|layer._runner|dashboard" | grep -v grep | awk '{print $2, $11, $12}'
echo "=== Dashboard ==="
curl -s -o /dev/null -w "HTTP %{http_code}\n" http://localhost:5556
echo "=== Main log ==="
tail -10 logs/main.log
