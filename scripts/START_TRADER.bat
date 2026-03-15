@echo off
echo Starting Prediction Trader...

REM Mount P: drive if not present
if not exist P:\ (
    echo Mounting P: drive...
    subst P: \\wsl.localhost\Ubuntu\home\andydoc\prediction-trader
    timeout /t 2 /nobreak >nul
)

wsl bash -c "cd /home/andydoc/prediction-trader && bash scripts/start.sh"
echo.
echo Dashboard: http://localhost:5556
echo.
pause
