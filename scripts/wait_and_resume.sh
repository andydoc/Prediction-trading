#!/bin/bash
# Wait 40 minutes then signal ready to continue Phase 6c
echo "$(date): Waiting 40 minutes (2400s) for session limit refresh..."
sleep 2400
echo "$(date): OK — ready to continue Phase 6c (L3→WS bridge)"
