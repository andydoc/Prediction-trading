#!/bin/bash
echo "=== Latest WS status ==="
grep 'WS' /home/andydoc/prediction-trader/logs/layer4_$(date +%Y%m%d).log | grep -v '< TEXT\|> TEXT\|PING\|PONG\|< Upgrade\|> GET\|> Host\|> Sec-\|> User-Agent\|< Sec-\|> Connec' | awk '/11:56:/{found=1} found' | tail -15
echo "=== Errors ==="
grep -i 'error\|warning' /home/andydoc/prediction-trader/logs/layer4_$(date +%Y%m%d).log | awk '/11:56:/{found=1} found' | grep -i 'ws' | tail -5
