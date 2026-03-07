#!/bin/bash
# ============================================================================
# VPS Remote Setup Script — run from your WSL terminal
# Usage: bash setup_remote_vps.sh
# ============================================================================
set -euo pipefail

VPS_IP="193.23.127.99"
VPS_USER="root"

echo "============================================"
echo "Prediction-Trader VPS Remote Setup"
echo "Target: ${VPS_USER}@${VPS_IP}"
echo "============================================"
echo ""
echo "You will be prompted for the root password."
echo ""

# Step 1: Copy SSH public key for passwordless access
echo "=== [1/3] Setting up SSH key access ==="
if [ ! -f ~/.ssh/id_rsa.pub ]; then
    echo "No SSH key found, generating one..."
    ssh-keygen -t rsa -b 2048 -f ~/.ssh/id_rsa -N ""
fi
ssh-copy-id -o StrictHostKeyChecking=no ${VPS_USER}@${VPS_IP}
echo "SSH key copied. Testing passwordless access..."
ssh -o BatchMode=yes ${VPS_USER}@${VPS_IP} "echo 'SSH key auth OK'"

# Step 2: Run setup on VPS
echo ""
echo "=== [2/3] Running setup on VPS ==="
ssh ${VPS_USER}@${VPS_IP} 'bash -s' << 'REMOTE_SCRIPT'
set -euo pipefail
LOG=/var/log/prediction-trader-setup.log
echo "=== Setup started $(date -u) ===" | tee $LOG

echo "=== [1/7] System packages ===" | tee -a $LOG
apt-get update -qq >> $LOG 2>&1
apt-get install -y -qq python3 python3-venv python3-dev python3-pip git curl wget htop jq build-essential libffi-dev libssl-dev pkg-config >> $LOG 2>&1
echo "  Python: $(python3 --version)" | tee -a $LOG

echo "=== [2/7] Swap (2GB) ===" | tee -a $LOG
if [ ! -f /swapfile ]; then
    fallocate -l 2G /swapfile
    chmod 600 /swapfile
    mkswap /swapfile >> $LOG 2>&1
    swapon /swapfile
    echo '/swapfile none swap sw 0 0' >> /etc/fstab
    echo "  Swap created" | tee -a $LOG
else
    echo "  Swap exists" | tee -a $LOG
fi
sysctl -w vm.swappiness=10 >> $LOG 2>&1
echo 'vm.swappiness=10' > /etc/sysctl.d/99-trader.conf

echo "=== [3/7] Clone repo ===" | tee -a $LOG
if [ -d /root/prediction-trader ]; then
    cd /root/prediction-trader && git pull --ff-only origin main >> $LOG 2>&1
    echo "  Repo updated" | tee -a $LOG
else
    git clone https://github.com/andydoc/Prediction-trading.git /root/prediction-trader >> $LOG 2>&1
    echo "  Repo cloned" | tee -a $LOG
fi
mkdir -p /root/prediction-trader/logs /root/prediction-trader/data/system_state /root/prediction-trader/data/resolution_cache

echo "=== [4/7] Python venv + packages ===" | tee -a $LOG
if [ ! -d /root/prediction-trader-env ]; then
    python3 -m venv /root/prediction-trader-env >> $LOG 2>&1
fi
/root/prediction-trader-env/bin/pip install --upgrade pip setuptools wheel -q >> $LOG 2>&1
/root/prediction-trader-env/bin/pip install -q pyyaml aiohttp requests numpy scipy cvxpy flask py-clob-client python-dateutil pytz tqdm >> $LOG 2>&1
/root/prediction-trader-env/bin/python -c "import yaml,aiohttp,requests,numpy,scipy,cvxpy,flask; print('  All imports OK')" | tee -a $LOG

echo "=== [5/7] Firewall ===" | tee -a $LOG
# Open ports if iptables exists
if command -v iptables &>/dev/null; then
    iptables -I INPUT -p tcp --dport 5556 -j ACCEPT 2>/dev/null || true
    iptables -I INPUT -p tcp --dport 5557 -j ACCEPT 2>/dev/null || true
    if [ -d /etc/iptables ]; then
        iptables-save > /etc/iptables/rules.v4 2>/dev/null || true
    fi
    echo "  Ports 5556,5557 opened" | tee -a $LOG
else
    echo "  No iptables, skipping" | tee -a $LOG
fi

echo "=== [6/7] Systemd service ===" | tee -a $LOG
cat > /etc/systemd/system/prediction-trader.service << 'EOF'
[Unit]
Description=Prediction Market Trader
After=network-online.target
Wants=network-online.target
[Service]
Type=simple
User=root
Group=root
WorkingDirectory=/root/prediction-trader
ExecStart=/root/prediction-trader-env/bin/python main.py
Restart=always
RestartSec=30
StandardOutput=append:/root/prediction-trader/logs/main.log
StandardError=append:/root/prediction-trader/logs/main.log
MemoryMax=3500M
CPUQuota=380%
Environment=PYTHONUNBUFFERED=1
[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload
systemctl enable prediction-trader >> $LOG 2>&1
echo "  Service created and enabled" | tee -a $LOG

echo "=== [7/7] Log rotation ===" | tee -a $LOG
cat > /etc/logrotate.d/prediction-trader << 'EOF'
/root/prediction-trader/logs/*.log {
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

echo "" | tee -a $LOG
echo "============================================" | tee -a $LOG
echo "VPS SETUP COMPLETE — $(date -u)" | tee -a $LOG
echo "Arch: $(uname -m) | RAM: $(free -h | awk '/Mem:/{print $2}') | Disk: $(df -h / | awk 'NR==2{print $4}') free" | tee -a $LOG
echo "Python: $(/root/prediction-trader-env/bin/python --version)" | tee -a $LOG
echo "============================================" | tee -a $LOG
REMOTE_SCRIPT

# Step 3: Copy secrets and state files
echo ""
echo "=== [3/3] Copying secrets and state files ==="
SRC="$HOME/prediction-trader"

# secrets.yaml
if [ -f "$SRC/config/secrets.yaml" ]; then
    scp "$SRC/config/secrets.yaml" ${VPS_USER}@${VPS_IP}:/root/prediction-trader/config/
    echo "  secrets.yaml copied"
else
    echo "  WARNING: secrets.yaml not found at $SRC/config/secrets.yaml"
fi

# .env
if [ -f "$SRC/.env" ]; then
    scp "$SRC/.env" ${VPS_USER}@${VPS_IP}:/root/prediction-trader/
    echo "  .env copied"
else
    echo "  WARNING: .env not found"
fi

# execution_state.json — fresh start with $100
echo '{"current_capital": 100.0, "initial_capital": 100.0, "open_positions": [], "closed_positions": [], "performance": {}}' | ssh ${VPS_USER}@${VPS_IP} "cat > /root/prediction-trader/data/system_state/execution_state.json"
echo "  Fresh execution_state.json created ($100 capital)"

# resolution_delay_p95.json
if [ -f "$SRC/data/resolution_delay_p95.json" ]; then
    scp "$SRC/data/resolution_delay_p95.json" ${VPS_USER}@${VPS_IP}:/root/prediction-trader/data/
    echo "  resolution_delay_p95.json copied"
fi

echo ""
echo "============================================"
echo "ALL DONE!"
echo "============================================"
echo ""
echo "To test:  ssh root@${VPS_IP} 'cd /root/prediction-trader && source /root/prediction-trader-env/bin/activate && python main.py'"
echo "To start: ssh root@${VPS_IP} 'systemctl start prediction-trader'"
echo "To check: ssh root@${VPS_IP} 'journalctl -u prediction-trader -f'"
echo "To logs:  ssh root@${VPS_IP} 'tail -f /root/prediction-trader/logs/main.log'"
echo ""
