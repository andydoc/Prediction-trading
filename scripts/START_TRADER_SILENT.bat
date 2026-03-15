@echo off
REM Silent start - used by Task Scheduler on login / Windows restart
REM 1. Ensures WSL is running
REM 2. Mounts P: drive
REM 3. Starts Rust supervisor via start.sh

REM Start WSL if not running (wsl -l exits 0 if running)
wsl -l >nul 2>&1 || wsl --exec echo "WSL started" >nul 2>&1
timeout /t 3 /nobreak >nul

REM Mount P: drive if not present
if not exist P:\ (
    subst P: \\wsl.localhost\Ubuntu\home\andydoc\prediction-trader
    timeout /t 2 /nobreak >nul
)

REM Start via start.sh (pulls code, builds if needed, starts supervisor)
wsl bash -c "cd /home/andydoc/prediction-trader && bash scripts/start.sh"
