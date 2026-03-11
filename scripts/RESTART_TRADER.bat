@echo off
echo Restarting Prediction Trader...
wsl bash -c "kill -9 \$(ps aux | grep -E 'main\.py|trading_engine|dashboard_server|initial_market_scanner' | grep -v grep | awk '{print \$2}') 2>/dev/null; sleep 3; rm -f /home/andydoc/prediction-trader/*.pid"
echo Pulling latest code...
wsl bash -c "cd /home/andydoc/prediction-trader && git pull --ff-only origin main 2>&1 || echo 'WARNING: git pull failed - continuing with local code'"
echo Starting system...
wsl bash -c "source /home/andydoc/prediction-trader-env/bin/activate && cd /home/andydoc/prediction-trader && nohup python main.py > logs/main.log 2>&1 &"
timeout /t 12 /nobreak >nul
echo.
echo === Process check ===
wsl bash -c "ps aux | grep -E 'main\.py|trading_engine|dashboard_server' | grep -v grep | awk '{print \$2, \$NF}'"
echo.
echo === Trading Engine startup log (last 10 lines) ===
wsl bash -c "tail -10 /home/andydoc/prediction-trader/logs/trading_engine_$(date +%%Y%%m%%d).log 2>/dev/null || echo 'Log not yet created'"
echo.
echo Dashboard: http://localhost:5556
echo.
pause
