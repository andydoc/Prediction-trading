#!/bin/bash
# OCI ARM Instance Auto-Retry - All values filled in
set -euo pipefail
export PATH=$HOME/bin:$PATH

COMPARTMENT_OCID="ocid1.tenancy.oc1..aaaaaaaalbioqee43vqlnrqpigdk2qkmjk33rgjwc6yyedqoc44vhkokuucq"
SUBNET_OCID="ocid1.subnet.oc1.eu-madrid-1.aaaaaaaai5ud6vfmz3ptemtcjngomgsomsol3gbz3qq5j5cgjpmkf62pg5na"
IMAGE_OCID="ocid1.image.oc1.eu-madrid-1.aaaaaaaalz3c6ahj2ecfy3q3iywqgtxs7fdoc5isnrcbwc3e4f7cgl3i43ya"
AVAILABILITY_DOMAIN="Dcka:EU-MADRID-1-AD-1"
INSTANCE_NAME="PT01"
SHAPE="VM.Standard.A1.Flex"
OCPUS=1
MEMORY_GB=6
RETRY_INTERVAL=60
SSH_KEY_FILE="$HOME/.ssh/oci_pt01.pub"

# Write SSH key to file
mkdir -p $HOME/.ssh
cat > "$SSH_KEY_FILE" << 'KEYEOF'
ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQCkWXEuPLj4Tc47+Ga9GgwIKz95ukgXO1AD/iThZmo8um8kH2zYXuZ2nQiyd5lfo1arkrruKMNDQwmdFwZBiIeEkrrRZzk3TNKTAM3IBLjVBUdXb8VL5Vs7gn+mM+/I/nH2EUa2BvF5Y+/b4OiWbf4gikOAYOP1vC4Z4ntXrlU3PiUZGymb9ZQJD0gTUFZuLLwiPV8kGdSbbVTVOmQRmW6AJPxmZ4L+P2oDbGx7f9ThjHns/bot2qf2S/vcIEWaW1o+mvc/vc7p0kxMbZ+L9cRoNoMiCuEA9R+hIIfPfrKEUZ6bcV2ENwZ0wHQ3YOxmJPxquM492GPSasf/QPERRPSp ssh-key-2026-03-06
KEYEOF

# Cloud-init script inline
CLOUD_INIT=$(mktemp)
cat > "$CLOUD_INIT" << 'CIEOF'
#!/bin/bash
set -euo pipefail
LOG=/var/log/prediction-trader-setup.log
echo "=== [1/7] System packages ===" >> $LOG 2>&1
apt-get update -qq >> $LOG 2>&1
apt-get install -y -qq python3 python3-venv python3-dev python3-pip git curl wget htop jq build-essential libffi-dev libssl-dev pkg-config >> $LOG 2>&1
echo "=== [2/7] Swap ===" >> $LOG 2>&1
fallocate -l 2G /swapfile && chmod 600 /swapfile && mkswap /swapfile && swapon /swapfile >> $LOG 2>&1
echo '/swapfile none swap sw 0 0' >> /etc/fstab
sysctl -w vm.swappiness=10 >> $LOG 2>&1
echo 'vm.swappiness=10' > /etc/sysctl.d/99-trader.conf
echo "=== [3/7] Clone ===" >> $LOG 2>&1
sudo -u ubuntu git clone https://github.com/andydoc/Prediction-trading.git /home/ubuntu/prediction-trader >> $LOG 2>&1
sudo -u ubuntu mkdir -p /home/ubuntu/prediction-trader/logs /home/ubuntu/prediction-trader/data/system_state /home/ubuntu/prediction-trader/data/resolution_cache
echo "=== [4/7] Venv ===" >> $LOG 2>&1
sudo -u ubuntu python3 -m venv /home/ubuntu/prediction-trader-env >> $LOG 2>&1
sudo -u ubuntu /home/ubuntu/prediction-trader-env/bin/pip install --upgrade pip setuptools wheel -q >> $LOG 2>&1
sudo -u ubuntu /home/ubuntu/prediction-trader-env/bin/pip install -q pyyaml aiohttp requests numpy scipy cvxpy flask py-clob-client python-dateutil pytz tqdm >> $LOG 2>&1
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
CIEOF

SHAPE_CONFIG="{\"ocpus\": ${OCPUS}, \"memoryInGBs\": ${MEMORY_GB}}"
attempt=0
start_time=$(date +%s)

echo "============================================"
echo "OCI ARM Instance Auto-Retry"
echo "Instance: ${INSTANCE_NAME} | ${OCPUS} OCPU, ${MEMORY_GB}GB"
echo "AD: ${AVAILABILITY_DOMAIN}"
echo "Retry every ${RETRY_INTERVAL}s | Started: $(date)"
echo "============================================"

while true; do
    attempt=$((attempt + 1))
    elapsed=$(( $(date +%s) - start_time ))
    hours=$((elapsed / 3600))
    mins=$(( (elapsed % 3600) / 60 ))

    echo "[$(date '+%H:%M:%S')] Attempt #${attempt} (${hours}h ${mins}m elapsed)"

    RESULT=$(oci compute instance launch \
        --compartment-id "${COMPARTMENT_OCID}" \
        --availability-domain "${AVAILABILITY_DOMAIN}" \
        --shape "${SHAPE}" \
        --shape-config "${SHAPE_CONFIG}" \
        --subnet-id "${SUBNET_OCID}" \
        --image-id "${IMAGE_OCID}" \
        --assign-public-ip true \
        --display-name "${INSTANCE_NAME}" \
        --ssh-authorized-keys-file "${SSH_KEY_FILE}" \
        --user-data-file "${CLOUD_INIT}" \
        2>&1) && EXIT_CODE=$? || EXIT_CODE=$?

    if [ $EXIT_CODE -eq 0 ]; then
        echo ""
        echo "============================================"
        echo "SUCCESS after ${attempt} attempts (${hours}h ${mins}m)!"
        echo "============================================"
        echo "${RESULT}" | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    inst = d.get('data', d)
    print(f'Instance ID: {inst.get(\"id\", \"?\")[:60]}...')
    print(f'State: {inst.get(\"lifecycle-state\", \"?\")}')
    print(f'Created: {inst.get(\"time-created\", \"?\")}')
except:
    pass
" 2>/dev/null
        echo ""
        echo "Waiting 30s for IP assignment..."
        sleep 30
        INSTANCE_ID=$(echo "${RESULT}" | python3 -c "import sys,json; print(json.load(sys.stdin).get('data',{}).get('id',''))" 2>/dev/null)
        if [ -n "${INSTANCE_ID}" ]; then
            VNIC_ID=$(oci compute vnic-attachment list --compartment-id "${COMPARTMENT_OCID}" --instance-id "${INSTANCE_ID}" --query 'data[0]."vnic-id"' --raw-output 2>/dev/null)
            if [ -n "${VNIC_ID}" ]; then
                PUBLIC_IP=$(oci network vnic get --vnic-id "${VNIC_ID}" --query 'data."public-ip"' --raw-output 2>/dev/null)
                echo "PUBLIC IP: ${PUBLIC_IP}"
                echo ""
                echo "SSH: ssh ubuntu@${PUBLIC_IP}"
                echo "Setup log: ssh ubuntu@${PUBLIC_IP} 'cat /var/log/prediction-trader-setup.log'"
            fi
        fi
        rm -f "$CLOUD_INIT"
        echo -e "\a"
        exit 0
    fi

    if echo "${RESULT}" | grep -qi "out of capacity\|out of host capacity"; then
        echo "  -> Out of capacity. Retrying in ${RETRY_INTERVAL}s..."
    elif echo "${RESULT}" | grep -qi "incorrectly formatted"; then
        echo "  -> Format error. Check cloud-init script."
        echo "  -> ${RESULT}" | head -3
    else
        echo "  -> Error: $(echo "${RESULT}" | head -2)"
    fi

    sleep ${RETRY_INTERVAL}
done
