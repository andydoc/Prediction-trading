@echo off
powershell -ExecutionPolicy Bypass -Command "& {. '\\wsl.localhost\Ubuntu\home\andydoc\prediction-trader\scripts\monitor_positions.ps1'}"
pause
