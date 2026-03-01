@echo off
REM Silent start - used by Task Scheduler on login
wsl bash -c "source /home/andydoc/prediction-trader-env/bin/activate && cd /home/andydoc/prediction-trader && rm -f *.pid && nohup python main.py > logs/main.log 2>&1 &"
