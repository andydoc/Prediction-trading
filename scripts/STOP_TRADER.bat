@echo off
echo Stopping Prediction Trader (all processes)...
wsl bash -c "kill -9 $(ps aux | grep 'main.py\|layer[1-4]_runner\|dashboard_server' | grep -v grep | awk '{print $2}') 2>/dev/null; sleep 2; rm -f /home/andydoc/prediction-trader/*.pid"
echo.
echo === State snapshot ===
wsl python3 -c "import json; d=json.load(open('/home/andydoc/prediction-trader/data/system_state/execution_state.json')); print(f'Capital: \${d[\"current_capital\"]:.2f}  Open: {len(d.get(\"open_positions\",[]))}  Closed: {len(d.get(\"closed_positions\",[]))}')"
echo.
echo === Remaining python processes ===
wsl bash -c "ps aux | grep python | grep -v grep || echo None"
echo.
pause
