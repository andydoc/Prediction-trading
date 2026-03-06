#!/bin/bash
# ============================================================================
# PREDICTION-TRADER: Oracle Cloud ARM A1 Cloud-Init Script
# Paste this into: Create Instance > Show advanced options > Management
# Target: Canonical Ubuntu 22.04 aarch64 Minimal, 1 OCPU Ampere, 6GB RAM
#
# AFTER PROVISIONING you must:
#   1. Add VCN Security List ingress rules for ports 5556, 5557
#   2. SSH in and copy secrets.yaml, .env, execution_state.json
#   3. Start the service: sudo systemctl start prediction-trader
# ============================================================================
set -euo pipefail
exec > >(tee /var/log/prediction-trader-setup.log) 2>&1

TRADER_USER="ubuntu"
TRADER_HOME="/home/${TRADER_USER}"
REPO_URL="https://github.com/andydoc/Prediction-trading.git"
REPO_DIR="${TRADER_HOME}/prediction-trader"
VENV_DIR="${TRADER_HOME}/prediction-trader-env"

echo "=== [1/7] System packages ==="
apt-get update -qq
apt-get install -y -qq \
    python3 python3-venv python3-dev python3-pip \
    git curl wget htop jq \
    build-essential libffi-dev libssl-dev pkg-config \
    > /dev/null 2>&1
echo "Python: $(python3 --version)"

echo "=== [2/7] Swap (2GB) ==="
if [ ! -f /swapfile ]; then
    fallocate -l 2G /swapfile
    chmod 600 /swapfile
    mkswap /swapfile > /dev/null
    swapon /swapfile
    echo '/swapfile none swap sw 0 0' >> /etc/fstab
fi
sysctl -w vm.swappiness=10 > /dev/null
echo 'vm.swappiness=10' > /etc/sysctl.d/99-trader.conf

echo "=== [3/7] Clone repo ==="
sudo -u ${TRADER_USER} git clone ${REPO_URL} ${REPO_DIR}
sudo -u ${TRADER_USER} mkdir -p \
    ${REPO_DIR}/logs \
    ${REPO_DIR}/data/system_state \
    ${REPO_DIR}/data/resolution_cache

echo "=== [4/7] Python venv + packages ==="
sudo -u ${TRADER_USER} python3 -m venv ${VENV_DIR}
sudo -u ${TRADER_USER} ${VENV_DIR}/bin/pip install --upgrade pip setuptools wheel -q
sudo -u ${TRADER_USER} ${VENV_DIR}/bin/pip install -q \
    pyyaml aiohttp requests numpy scipy cvxpy flask \
    py-clob-client python-dateutil pytz tqdm
sudo -u ${TRADER_USER} ${VENV_DIR}/bin/python -c \
    "import yaml,aiohttp,requests,numpy,scipy,cvxpy,flask; print('All imports OK')"

echo "=== [5/7] Firewall (iptables — OCI Ubuntu standard) ==="
# OCI Ubuntu uses /etc/iptables/rules.v4 — UFW is broken on OCI
# Insert rules BEFORE the REJECT line (line with icmp-host-prohibited)
iptables -I INPUT 5 -p tcp -m state --state NEW -m tcp --dport 5556 -j ACCEPT
iptables -I INPUT 6 -p tcp -m state --state NEW -m tcp --dport 5557 -j ACCEPT
# Persist — write back to rules.v4
iptables-save > /etc/iptables/rules.v4

echo "=== [6/7] Systemd service ==="
cat > /etc/systemd/system/prediction-trader.service << 'SVCEOF'
[Unit]
Description=Prediction Market Arbitrage Trader
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
SVCEOF
systemctl daemon-reload
systemctl enable prediction-trader

echo "=== [7/7] Log rotation ==="
cat > /etc/logrotate.d/prediction-trader << 'LOGEOF'
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
LOGEOF

echo ""
echo "============================================"
echo "SETUP COMPLETE — $(date -u)"
echo "============================================"
echo "Arch: $(uname -m) | RAM: $(free -h | awk '/Mem:/{print $2}') | Swap: 2G"
echo "Python: $(${VENV_DIR}/bin/python --version)"
echo "Repo: ${REPO_DIR}"
echo "Venv: ${VENV_DIR}"
echo ""
echo "NEXT STEPS (SSH in as ubuntu):"
echo "  1. scp secrets.yaml and .env to ${REPO_DIR}/config/"
echo "  2. scp execution_state.json to ${REPO_DIR}/data/system_state/"
echo "  3. scp resolution_delay_p95.json to ${REPO_DIR}/data/"
echo "  4. Add VCN Security List ingress: TCP 5556, 5557 from your IP"
echo "  5. Test: cd ${REPO_DIR} && source ${VENV_DIR}/bin/activate && python main.py"
echo "  6. Start: sudo systemctl start prediction-trader"
echo "  7. Check: sudo journalctl -u prediction-trader -f"
echo ""
echo "Setup log: /var/log/prediction-trader-setup.log"
