@echo off
echo Starting Prediction Trader...
wsl bash -c "source /home/andydoc/prediction-trader-env/bin/activate && cd /home/andydoc/prediction-trader && rm -f *.pid && nohup python main.py > logs/main.log 2>&1 &"
timeout /t 12 /nobreak >nul
echo Checking status...
wsl bash -c "ps aux | grep -E 'main\.py|dashboard_server' | grep -v grep | wc -l"
echo.
echo Dashboard: http://localhost:5556
echo.
pause
