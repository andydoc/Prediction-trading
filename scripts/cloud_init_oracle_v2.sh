#!/bin/bash
set -euo pipefail
LOG=/var/log/prediction-trader-setup.log
TRADER_USER="ubuntu"
REPO_URL="https://github.com/andydoc/Prediction-trading.git"
REPO_DIR="/home/ubuntu/prediction-trader"
VENV_DIR="/home/ubuntu/prediction-trader-env"
echo "=== [1/7] System packages ===" >> $LOG 2>&1
apt-get update -qq >> $LOG 2>&1
apt-get install -y -qq python3 python3-venv python3-dev python3-pip git curl wget htop jq build-essential libffi-dev libssl-dev pkg-config >> $LOG 2>&1
echo "=== [2/7] Swap ===" >> $LOG 2>&1
fallocate -l 2G /swapfile && chmod 600 /swapfile && mkswap /swapfile && swapon /swapfile >> $LOG 2>&1
echo '/swapfile none swap sw 0 0' >> /etc/fstab
sysctl -w vm.swappiness=10 >> $LOG 2>&1
echo 'vm.swappiness=10' > /etc/sysctl.d/99-trader.conf
echo "=== [3/7] Clone ===" >> $LOG 2>&1
sudo -u ubuntu git clone $REPO_URL $REPO_DIR >> $LOG 2>&1
sudo -u ubuntu mkdir -p $REPO_DIR/logs $REPO_DIR/data/system_state $REPO_DIR/data/resolution_cache
echo "=== [4/7] Venv ===" >> $LOG 2>&1
sudo -u ubuntu python3 -m venv $VENV_DIR >> $LOG 2>&1
sudo -u ubuntu $VENV_DIR/bin/pip install --upgrade pip setuptools wheel -q >> $LOG 2>&1
sudo -u ubuntu $VENV_DIR/bin/pip install -q pyyaml aiohttp requests numpy scipy cvxpy flask py-clob-client python-dateutil pytz tqdm >> $LOG 2>&1
echo "=== [5/7] Firewall ===" >> $LOG 2>&1
iptables -I INPUT 5 -p tcp -m state --state NEW -m tcp --dport 5556 -j ACCEPT >> $LOG 2>&1
iptables -I INPUT 6 -p tcp -m state --state NEW -m tcp --dport 5557 -j ACCEPT >> $LOG 2>&1
iptables-save > /etc/iptables/rules.v4
echo "=== [6/7] Systemd ===" >> $LOG 2>&1
cat > /etc/systemd/system/prediction-trader.service << 'EOF'
[Unit]
Description=Prediction Market Trader
After=network-online.target
Wants=network-online.target
[Service]
Type=simple
User=ubuntu
Group=ubuntu
WorkingDirectory=/home/ubuntu/prediction-trader
ExecStart=/home/ubuntu/prediction-trader-env/bin/python main.py
Restart=always
RestartSec=30
StandardOutput=append:/home/ubuntu/prediction-trader/logs/main.log
StandardError=append:/home/ubuntu/prediction-trader/logs/main.log
MemoryMax=5G
CPUQuota=95%
Environment=PYTHONUNBUFFERED=1
[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload >> $LOG 2>&1
systemctl enable prediction-trader >> $LOG 2>&1
echo "=== [7/7] Logrotate ===" >> $LOG 2>&1
cat > /etc/logrotate.d/prediction-trader << 'EOF'
/home/ubuntu/prediction-trader/logs/*.log {
    daily
    rotate 7
    compress
    delaycompress
    missingok
    notifempty
    copytruncate
    maxsize 100M
}
EOF
echo "=== SETUP COMPLETE ===" >> $LOG 2>&1