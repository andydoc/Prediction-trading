@echo off
echo Stopping Prediction Trader...
wsl bash -c "cd /home/andydoc/prediction-trader && bash scripts/kill.sh"
echo.
pause
