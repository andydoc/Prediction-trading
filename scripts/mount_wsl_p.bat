@echo off
REM Mount WSL prediction-trader repo as P: drive
REM Copy this to: C:\Users\<user>\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Startup\
REM Runs automatically on Windows login. NOT needed on VPS/Linux.
subst P: \\wsl.localhost\Ubuntu\home\andydoc\prediction-trader
