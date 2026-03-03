@echo off
REM Silent start - used by Task Scheduler on login
REM Pull latest code then start
wsl bash -c "cd /home/andydoc/prediction-trader && git pull --ff-only origin main 2>/dev/null; source /home/andydoc/prediction-trader-env/bin/activate && rm -f *.pid && nohup python main.py > logs/main.log 2>&1 &"
