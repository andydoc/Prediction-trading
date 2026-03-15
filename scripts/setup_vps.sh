#!/bin/bash
#===============================================================================
# PREDICTION-TRADER VPS INITIALIZATION SCRIPT
# Target: Oracle Cloud Always Free — ARM Ampere A1 (1 OCPU, 6GB RAM)
# OS: Ubuntu 22.04/24.04 aarch64
#
# Usage:
#   1. Create Oracle ARM A1 instance (1 OCPU, 6GB, 50GB boot, Ubuntu)
#   2. SSH in: ssh -i <key> ubuntu@<ip>
#   3. Upload this script: scp setup_vps.sh ubuntu@<ip>:~/
#   4. Run: chmod +x setup_vps.sh && sudo ./setup_vps.sh
#   5. Then as 'trader' user: run the post-install steps at the bottom
#
# What this does:
#   - Creates 'trader' user with SSH access
#   - Installs Python 3.12+, git, system dependencies
#   - Clones the repo, creates venv, installs pip packages
#   - Creates systemd service for auto-start on reboot
#   - Sets up log rotation
#   - Opens firewall ports for dashboard (5556) and exec control (5557)
#   - Sets up 2GB swap (ARM instances have no swap by default)
#
# What you do manually after:
#   - Copy secrets.yaml and .env from laptop
#   - Configure execution_control to coordinate with laptop
#   - First test run
#===============================================================================

set -euo pipefail

# --- Configuration ---
TRADER_USER="trader"
REPO_URL="https://github.com/andydoc/Prediction-trading.git"
REPO_DIR="/home/${TRADER_USER}/prediction-trader"
VENV_DIR="/home/${TRADER_USER}/prediction-trader-env"
PYTHON_VERSION="python3"  # Will use system python3 (3.10+ on Ubuntu 22.04)

echo "========================================"
echo "PREDICTION-TRADER VPS SETUP"
echo "========================================"
echo "Target: Oracle ARM A1 (1 OCPU, 6GB RAM)"
echo "OS: $(lsb_release -ds 2>/dev/null || cat /etc/os-release | head -1)"
echo "Arch: $(uname -m)"
echo "========================================"
echo ""

# --- 0. Verify ARM ---
ARCH=$(uname -m)
if [[ "$ARCH" != "aarch64" && "$ARCH" != "x86_64" ]]; then
    echo "WARNING: Unexpected architecture: $ARCH"
    echo "This script is designed for aarch64 (ARM) or x86_64"
    read -p "Continue anyway? [y/N] " -n 1 -r
    echo
    [[ ! $REPLY =~ ^[Yy]$ ]] && exit 1
fi

# --- 1. System packages ---
echo "[1/8] Installing system packages..."
apt-get update -qq
apt-get install -y -qq \
    ${PYTHON_VERSION} \
    ${PYTHON_VERSION}-venv \
    ${PYTHON_VERSION}-dev \
    python3-pip \
    git \
    curl \
    wget \
    htop \
    jq \
    build-essential \
    libffi-dev \
    libssl-dev \
    pkg-config \
    > /dev/null 2>&1
echo "  Done. Python: $(${PYTHON_VERSION} --version)"

# Install Rust toolchain (needed for Rust engine + supervisor)
if ! command -v cargo &>/dev/null; then
    echo "  Installing Rust toolchain..."
    sudo -u ${TRADER_USER} bash -c 'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y'
    echo "  Rust installed"
fi

# --- 2. Create trader user ---
echo "[2/8] Setting up trader user..."
if id "${TRADER_USER}" &>/dev/null; then
    echo "  User '${TRADER_USER}' already exists"
else
    useradd -m -s /bin/bash "${TRADER_USER}"
    echo "  Created user '${TRADER_USER}'"
fi

# Copy SSH keys from ubuntu user for SSH access
if [ -d /home/ubuntu/.ssh ]; then
    mkdir -p /home/${TRADER_USER}/.ssh
    cp /home/ubuntu/.ssh/authorized_keys /home/${TRADER_USER}/.ssh/ 2>/dev/null || true
    chown -R ${TRADER_USER}:${TRADER_USER} /home/${TRADER_USER}/.ssh
    chmod 700 /home/${TRADER_USER}/.ssh
    chmod 600 /home/${TRADER_USER}/.ssh/authorized_keys 2>/dev/null || true
    echo "  SSH keys copied from ubuntu user"
fi

# Add trader to sudoers (limited)
echo "${TRADER_USER} ALL=(ALL) NOPASSWD: /bin/systemctl restart prediction-trader, /bin/systemctl stop prediction-trader, /bin/systemctl status prediction-trader, /bin/journalctl" > /etc/sudoers.d/trader
chmod 440 /etc/sudoers.d/trader

# --- 3. Setup swap (ARM instances often have none) ---
echo "[3/8] Configuring swap..."
if [ ! -f /swapfile ]; then
    fallocate -l 2G /swapfile
    chmod 600 /swapfile
    mkswap /swapfile > /dev/null
    swapon /swapfile
    echo '/swapfile none swap sw 0 0' >> /etc/fstab
    echo "  Created 2GB swap"
else
    echo "  Swap already exists"
fi
# Tune swappiness for trading (prefer keeping processes in RAM)
sysctl vm.swappiness=10 > /dev/null
echo 'vm.swappiness=10' > /etc/sysctl.d/99-trader.conf

# --- 4. Clone repo ---
echo "[4/8] Cloning repository..."
if [ -d "${REPO_DIR}" ]; then
    echo "  Repo already exists, pulling latest..."
    sudo -u ${TRADER_USER} git -C "${REPO_DIR}" pull --ff-only origin main || true
else
    sudo -u ${TRADER_USER} git clone "${REPO_URL}" "${REPO_DIR}"
    echo "  Cloned to ${REPO_DIR}"
fi

# Create required directories
sudo -u ${TRADER_USER} mkdir -p \
    "${REPO_DIR}/logs" \
    "${REPO_DIR}/data/system_state" \
    "${REPO_DIR}/data/resolution_cache"

# Build Rust supervisor binary
echo "  Building Rust supervisor binary..."
sudo -u ${TRADER_USER} bash -c "source ~/.cargo/env && cd ${REPO_DIR}/rust_supervisor && cargo build --release"
echo "  Supervisor binary: ${REPO_DIR}/rust_supervisor/target/release/prediction-trader"

# --- 5. Python venv + dependencies ---
echo "[5/8] Setting up Python environment..."
if [ ! -d "${VENV_DIR}" ]; then
    sudo -u ${TRADER_USER} ${PYTHON_VERSION} -m venv "${VENV_DIR}"
    echo "  Created venv at ${VENV_DIR}"
fi

# Upgrade pip first
sudo -u ${TRADER_USER} "${VENV_DIR}/bin/pip" install --upgrade pip setuptools wheel -q

# Install core dependencies
echo "  Installing pip packages (this takes 2-5 minutes on ARM)..."
sudo -u ${TRADER_USER} "${VENV_DIR}/bin/pip" install -q \
    pyyaml \
    aiohttp \
    requests \
    numpy \
    scipy \
    cvxpy \
    flask \
    py-clob-client \
    python-dateutil \
    pytz \
    tqdm \
    maturin

# Build Rust engine as PyO3 module
echo "  Building Rust engine (maturin develop)..."
sudo -u ${TRADER_USER} bash -c "source ~/.cargo/env && source ${VENV_DIR}/bin/activate && cd ${REPO_DIR}/rust_engine && maturin develop --release"

# Verify critical imports
echo "  Verifying imports..."
sudo -u ${TRADER_USER} "${VENV_DIR}/bin/python" -c "
import yaml, aiohttp, requests, numpy, scipy, cvxpy, flask
from py_clob_client.client import ClobClient
print('All critical imports OK')
print(f'  numpy={numpy.__version__}, scipy={scipy.__version__}, cvxpy={cvxpy.__version__}')
" || {
    echo "CRITICAL: Import verification failed!"
    echo "Check logs above for missing packages"
    exit 1
}

# --- 6. Firewall ---
echo "[6/8] Configuring firewall..."
# Oracle Cloud uses iptables by default
if command -v ufw &>/dev/null; then
    ufw allow 22/tcp > /dev/null 2>&1    # SSH
    ufw allow 5556/tcp > /dev/null 2>&1  # Dashboard
    ufw allow 5557/tcp > /dev/null 2>&1  # Execution control
    ufw --force enable > /dev/null 2>&1
    echo "  UFW configured (22, 5556, 5557)"
else
    # Direct iptables for Oracle Cloud
    iptables -I INPUT -p tcp --dport 5556 -j ACCEPT 2>/dev/null || true
    iptables -I INPUT -p tcp --dport 5557 -j ACCEPT 2>/dev/null || true
    # Persist rules
    if command -v netfilter-persistent &>/dev/null; then
        netfilter-persistent save > /dev/null 2>&1
    fi
    echo "  iptables rules added (5556, 5557)"
fi
echo "  NOTE: Also add ingress rules in Oracle Cloud Console > Security Lists!"

# --- 7. Systemd service ---
echo "[7/8] Creating systemd service..."
cat > /etc/systemd/system/prediction-trader.service << SVCEOF
[Unit]
Description=Prediction Market Arbitrage Trader
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=${TRADER_USER}
Group=${TRADER_USER}
WorkingDirectory=${REPO_DIR}
ExecStart=${REPO_DIR}/rust_supervisor/target/release/prediction-trader --workspace ${REPO_DIR} --python ${VENV_DIR}/bin/python
Restart=always
RestartSec=30

# Resource limits
MemoryMax=5G
CPUQuota=95%

# Environment
Environment=PYTHONUNBUFFERED=1
Environment=PATH=/home/${TRADER_USER}/.cargo/bin:/usr/local/bin:/usr/bin:/bin

[Install]
WantedBy=multi-user.target
SVCEOF

systemctl daemon-reload
systemctl enable prediction-trader > /dev/null 2>&1
echo "  Service created and enabled"

# --- 8. Log rotation ---
echo "[8/8] Configuring log rotation..."
cat > /etc/logrotate.d/prediction-trader << LOGEOF
${REPO_DIR}/logs/*.log {
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
echo "  Logrotate configured (7 days, max 100MB per file)"

# --- Summary ---
echo ""
echo "========================================"
echo "SETUP COMPLETE"
echo "========================================"
echo ""
echo "System:"
echo "  User:    ${TRADER_USER}"
echo "  Repo:    ${REPO_DIR}"
echo "  Venv:    ${VENV_DIR}"
echo "  Service: prediction-trader.service"
echo "  Python:  $(${VENV_DIR}/bin/python --version)"
echo "  Arch:    $(uname -m)"
echo "  RAM:     $(free -h | awk '/Mem:/{print $2}')"
echo "  Swap:    $(free -h | awk '/Swap:/{print $2}')"
echo "  Disk:    $(df -h / | awk 'NR==2{print $4}') free"
echo ""
echo "========================================"
echo "MANUAL STEPS REQUIRED"
echo "========================================"
echo ""
echo "1. Switch to trader user:"
echo "   sudo su - ${TRADER_USER}"
echo ""
echo "2. Copy secrets from laptop:"
echo "   # On laptop:"
echo "   scp ~/.../prediction-trader/config/secrets.yaml ${TRADER_USER}@<VPS_IP>:${REPO_DIR}/config/"
echo "   scp ~/.../prediction-trader/.env ${TRADER_USER}@<VPS_IP>:${REPO_DIR}/"
echo ""
echo "3. Create initial execution state (or copy from laptop):"
echo "   # Fresh start with \$100:"
echo '   echo '"'"'{"current_capital": 100.0, "initial_capital": 100.0, "open_positions": [], "closed_positions": [], "performance": {}}'"'"' > ${REPO_DIR}/data/system_state/execution_state.json'
echo ""
echo "   # OR copy from laptop:"
echo "   scp ~/.../data/system_state/execution_state.json ${TRADER_USER}@<VPS_IP>:${REPO_DIR}/data/system_state/"
echo ""
echo "4. Copy resolution delay table:"
echo "   scp ~/.../data/resolution_delay_p95.json ${TRADER_USER}@<VPS_IP>:${REPO_DIR}/data/"
echo ""
echo "5. Oracle Cloud Console — add Security List ingress rules:"
echo "   - Port 5556 TCP (dashboard) — restrict to your IP"
echo "   - Port 5557 TCP (exec control) — restrict to your IP"
echo ""
echo "6. Configure execution control (choose one):"
echo "   a) VPS is leader (laptop monitors only):"
echo "      # Edit config.yaml on laptop:"
echo "      # execution_control.url: http://<VPS_IP>:5557"
echo ""
echo "   b) Laptop remains leader (VPS monitors only):"
echo "      # Edit config.yaml on VPS:"
echo "      # execution_control.url: http://<LAPTOP_IP>:5557"
echo ""
echo "7. Test run:"
echo "   sudo su - ${TRADER_USER}"
echo "   cd ${REPO_DIR}"
echo "   ./rust_supervisor/target/release/prediction-trader --dry-run  # Verify config"
echo "   ./rust_supervisor/target/release/prediction-trader  # Watch for errors, Ctrl+C to stop"
echo ""
echo "8. Start as service:"
echo "   sudo systemctl start prediction-trader"
echo "   sudo systemctl status prediction-trader"
echo "   tail -f ${REPO_DIR}/logs/main.log"
echo ""
echo "9. Useful commands:"
echo "   sudo systemctl restart prediction-trader  # Restart"
echo "   sudo systemctl stop prediction-trader      # Stop"
echo "   sudo journalctl -u prediction-trader -f    # Live logs"
echo "   htop                                       # Resource monitor"
echo ""
