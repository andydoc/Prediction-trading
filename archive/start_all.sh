#!/bin/bash
cd /home/andydoc/prediction-trader
/home/andydoc/prediction-trader-env/bin/python layer1_runner.py &
sleep 1
/home/andydoc/prediction-trader-env/bin/python layer2_runner.py &
sleep 1
/home/andydoc/prediction-trader-env/bin/python layer3_runner.py &
sleep 1
/home/andydoc/prediction-trader-env/bin/python layer4_runner.py &
echo "All layers started. Check: ps aux | grep layer"
