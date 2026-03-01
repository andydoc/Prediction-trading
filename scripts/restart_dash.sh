#!/bin/bash
# Kill dashboard and let supervisor restart it
kill $(ps aux | grep dashboard_server | grep -v grep | awk '{print $2}') 2>/dev/null
echo "Dashboard killed, supervisor will restart in ~5s"
sleep 6
# Check it's back
curl -s -o /dev/null -w "Dashboard HTTP status: %{http_code}\n" http://localhost:5556/
