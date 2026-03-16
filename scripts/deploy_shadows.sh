#!/usr/bin/env bash
# Deploy 5 shadow instances to VPS
# Usage: ./scripts/deploy_shadows.sh [start|stop|status|restart]

set -euo pipefail

INSTANCES=(shadow-a shadow-b shadow-c shadow-d shadow-e)
ACTION=${1:-status}

case "$ACTION" in
  install)
    echo "Installing systemd template..."
    sudo cp scripts/systemd/prediction-trader@.service /etc/systemd/system/
    sudo systemctl daemon-reload
    for inst in "${INSTANCES[@]}"; do
      sudo systemctl enable "prediction-trader@${inst}"
    done
    echo "Done. Use: $0 start"
    ;;
  start)
    for inst in "${INSTANCES[@]}"; do
      echo "Starting $inst..."
      sudo systemctl start "prediction-trader@${inst}"
    done
    ;;
  stop)
    for inst in "${INSTANCES[@]}"; do
      echo "Stopping $inst..."
      sudo systemctl stop "prediction-trader@${inst}"
    done
    ;;
  restart)
    for inst in "${INSTANCES[@]}"; do
      echo "Restarting $inst..."
      sudo systemctl restart "prediction-trader@${inst}"
    done
    ;;
  status)
    for inst in "${INSTANCES[@]}"; do
      status=$(systemctl is-active "prediction-trader@${inst}" 2>/dev/null || echo "inactive")
      port=$(grep -oP 'port: \K\d+' "config/instances/${inst}.yaml" 2>/dev/null || echo "?")
      printf "%-12s  %-10s  port=%s\n" "$inst" "$status" "$port"
    done
    ;;
  logs)
    inst=${2:-shadow-a}
    journalctl -u "prediction-trader@${inst}" -f
    ;;
  *)
    echo "Usage: $0 {install|start|stop|restart|status|logs [instance]}"
    ;;
esac
