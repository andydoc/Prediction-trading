#!/bin/bash
cd /home/andydoc/prediction-trader
rm -f *.pid
nohup python3 main.py > logs/main.log 2>&1 &
echo "System started PID=$!"
sleep 8
ps aux | grep -E "main.py|dashboard_server" | grep -v grep | awk '{print $2, $NF}'
