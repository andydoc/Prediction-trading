#!/bin/bash
echo "Killing all trader processes..."
kill $(ps aux | grep 'main.py\|layer[1-4]_runner\|dashboard_server' | grep -v grep | awk '{print $2}') 2>/dev/null
sleep 3

echo "Starting system..."
source /home/andydoc/prediction-trader-env/bin/activate
cd /home/andydoc/prediction-trader
rm -f *.pid
nohup python main.py > logs/main.log 2>&1 &
echo "Started PID: $!"

sleep 12
echo ""
echo "=== Process check ==="
ps aux | grep 'main.py\|layer[1-4]_runner\|dashboard_server' | grep -v grep | awk '{print $2, $NF}'

echo ""
echo "=== L4 startup log ==="
tail -15 logs/layer4_$(date +%Y%m%d).log

echo ""
echo "=== Dashboard check ==="
curl -s -o /dev/null -w "HTTP %{http_code}\n" http://localhost:5556/
