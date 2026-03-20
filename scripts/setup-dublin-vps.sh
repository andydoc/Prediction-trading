#!/usr/bin/env bash
# =============================================================================
# Dublin VPS Setup Script
# Run as root on a fresh Ubuntu 24 instance from is*hosting (Dublin/Interxion)
#
# Usage:
#   ssh root@<DUBLIN_IP> 'bash -s' < scripts/setup-dublin-vps.sh
#
# After this script: SCP secrets.yaml + state DB, then deploy the binary.
# =============================================================================
set -euo pipefail

TRADE_USER="ubuntu"
PROJECT_DIR="/home/${TRADE_USER}/prediction-trader"

echo "=== Dublin VPS Setup ==="
echo "$(date -u +%Y-%m-%dT%H:%M:%SZ)"

# --- 1. System updates & essentials ---
echo ">>> Updating system..."
apt-get update -qq
apt-get upgrade -y -qq
apt-get install -y -qq \
  build-essential pkg-config libssl-dev \
  git curl wget htop tmux unzip jq \
  sqlite3 libsqlite3-dev \
  fail2ban ufw

# --- 2. Create trade user ---
echo ">>> Creating user: ${TRADE_USER}"
if ! id "${TRADE_USER}" &>/dev/null; then
  useradd -m -s /bin/bash "${TRADE_USER}"
  # Allow SSH key login (copy root's authorized_keys)
  mkdir -p /home/${TRADE_USER}/.ssh
  cp /root/.ssh/authorized_keys /home/${TRADE_USER}/.ssh/ 2>/dev/null || true
  chown -R ${TRADE_USER}:${TRADE_USER} /home/${TRADE_USER}/.ssh
  chmod 700 /home/${TRADE_USER}/.ssh
  chmod 600 /home/${TRADE_USER}/.ssh/authorized_keys 2>/dev/null || true
  # Sudoers
  echo "${TRADE_USER} ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/${TRADE_USER}
fi

# --- 3. Firewall ---
echo ">>> Configuring firewall..."
ufw default deny incoming
ufw default allow outgoing
ufw allow ssh
ufw allow 5558/tcp   # Dashboard (main)
ufw allow 5570/tcp   # Dashboard (clob-test)
ufw --force enable

# --- 4. Install Rust toolchain ---
echo ">>> Installing Rust..."
su - ${TRADE_USER} -c 'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y'

# --- 5. Clone repo ---
echo ">>> Cloning repository..."
su - ${TRADE_USER} -c "
  source ~/.cargo/env
  if [ ! -d '${PROJECT_DIR}' ]; then
    git clone https://github.com/andydoc/Prediction-trading.git ${PROJECT_DIR}
  fi
  cd ${PROJECT_DIR}
  git fetch --all
  git checkout main
  git pull origin main
"

# --- 6. Create directory structure ---
echo ">>> Creating directories..."
su - ${TRADE_USER} -c "
  mkdir -p ${PROJECT_DIR}/config/instances
  mkdir -p ${PROJECT_DIR}/data/system_state
  mkdir -p ${PROJECT_DIR}/logs
"

# --- 7. Build release binaries ---
echo ">>> Building release binaries (this takes a few minutes)..."
su - ${TRADE_USER} -c "
  source ~/.cargo/env
  cd ${PROJECT_DIR}
  cargo build --release 2>&1 | tail -5
"

# --- 8. Systemd service for main trader ---
echo ">>> Installing systemd services..."
cat > /etc/systemd/system/prediction-trader.service << 'UNIT'
[Unit]
Description=Prediction Market Arbitrage Trader
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=ubuntu
WorkingDirectory=/home/ubuntu/prediction-trader
ExecStart=/home/ubuntu/prediction-trader/target/release/prediction-trader --workspace /home/ubuntu/prediction-trader
Restart=on-failure
RestartSec=30
MemoryMax=5G

# Logging
StandardOutput=journal
StandardError=journal
SyslogIdentifier=prediction-trader

[Install]
WantedBy=multi-user.target
UNIT

# Template service for instances (shadow-a etc)
cat > /etc/systemd/system/prediction-trader@.service << 'UNIT'
[Unit]
Description=Prediction Trader (%i)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=ubuntu
WorkingDirectory=/home/ubuntu/prediction-trader
ExecStart=/home/ubuntu/prediction-trader/target/release/prediction-trader --instance %i
Restart=on-failure
RestartSec=30
MemoryMax=5G

StandardOutput=journal
StandardError=journal
SyslogIdentifier=prediction-trader-%i

[Install]
WantedBy=multi-user.target
UNIT

systemctl daemon-reload
# Enable main service but DON'T start yet (needs secrets.yaml)
systemctl enable prediction-trader.service
# Do NOT enable template instances — the strategy tracker replaces them

# --- 9. Verify CLOB API is reachable from this IP ---
echo ">>> Testing Polymarket API access..."
GEOBLOCK_RESULT=$(curl -s -o /dev/null -w "%{http_code}" "https://gamma-api.polymarket.com/markets?limit=1" || echo "FAIL")
echo "Gamma API response: ${GEOBLOCK_RESULT}"
if [ "${GEOBLOCK_RESULT}" = "200" ]; then
  echo "✓ Polymarket API accessible from this IP"
else
  echo "✗ WARNING: Polymarket API returned ${GEOBLOCK_RESULT} — may be geoblocked!"
fi

CLOB_RESULT=$(curl -s -o /dev/null -w "%{http_code}" "https://clob.polymarket.com/book?token_id=0" || echo "FAIL")
echo "CLOB API response: ${CLOB_RESULT}"

# --- 10. Test latency to Polymarket ---
echo ">>> Latency test to Polymarket (eu-west-2)..."
for host in clob.polymarket.com gamma-api.polymarket.com; do
  AVG=$(ping -c 5 -q ${host} 2>/dev/null | tail -1 | awk -F/ '{print $5}')
  echo "  ${host}: ${AVG}ms avg"
done

# --- Summary ---
echo ""
echo "=== Setup Complete ==="
echo "Next steps:"
echo "  1. SCP secrets.yaml:  scp config/secrets.yaml ubuntu@<IP>:${PROJECT_DIR}/config/"
echo "  2. SCP state DB:      scp data/system_state/execution_state.db ubuntu@<IP>:${PROJECT_DIR}/data/system_state/"
echo "  3. Start trader:      ssh ubuntu@<IP> 'sudo systemctl start prediction-trader'"
echo "  4. Check dashboard:   http://<IP>:5558"
echo "  5. Run CLOB tests:    ssh ubuntu@<IP> 'cd ${PROJECT_DIR} && ./target/release/clob-test --workspace . --timeout-minutes 720'"
echo ""
echo "Shadow capital reset: All instances start fresh at \$1,000 (new state DB)"
echo ""
