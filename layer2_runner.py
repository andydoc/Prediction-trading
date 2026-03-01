"""Layer 2 Runner - Constraint Detection"""
import json, logging, sys, time, yaml
from pathlib import Path
from datetime import datetime
from zoneinfo import ZoneInfo

sys.path.append(str(Path(__file__).parent))
from layer2_constraint_detection.constraint_detector import ConstraintDetector
from layer1_market_data.market_data import MarketData

WORKSPACE        = Path('/home/andydoc/prediction-trader')
CONFIG_PATH      = WORKSPACE / 'config' / 'config.yaml'
INPUT_PATH       = WORKSPACE / 'data' / 'latest_markets.json'
OUTPUT_PATH      = WORKSPACE / 'layer2_constraint_detection' / 'data' / 'latest_constraints.json'
STATUS_PATH      = WORKSPACE / 'data' / 'layer2_status.json'

logging.basicConfig(level=logging.DEBUG,
    format='%(asctime)s - [L2] %(levelname)s - %(message)s',
    handlers=[logging.FileHandler(str(WORKSPACE / 'logs' / f'layer2_{datetime.now().strftime("%Y%m%d")}.log')), logging.StreamHandler()])
log = logging.getLogger('layer2')

def write_status(status, count=0, error=None):
    STATUS_PATH.parent.mkdir(parents=True, exist_ok=True)
    STATUS_PATH.write_text(json.dumps({'status': status, 'constraint_count': count, 'error': error, 'timestamp': datetime.now(ZoneInfo('Europe/London')).isoformat()}))

def main():
    with open(CONFIG_PATH) as f:
        config = yaml.safe_load(f)
    rebuild_interval = config.get('constraint_detection', {}).get('rebuild_interval', 300)
    detector = ConstraintDetector(config, WORKSPACE)
    log.info(f'Layer 2 started, rebuild_interval={rebuild_interval}s')
    write_status('starting')
    iteration = 0
    while True:
        iteration += 1
        try:
            if not INPUT_PATH.exists():
                log.warning(f'[iter {iteration}] No markets file yet')
                write_status('waiting_for_layer1')
                time.sleep(30)
                continue
            data = json.loads(INPUT_PATH.read_text())
            raw_markets = data.get('markets', [])
            if not raw_markets:
                log.warning(f'[iter {iteration}] Empty markets')
                write_status('waiting_for_layer1')
                time.sleep(30)
                continue
            log.info(f'[iter {iteration}] Detecting constraints on {len(raw_markets)} markets')
            write_status('running')
            markets = [MarketData.from_dict(m) for m in raw_markets]
            constraints = detector.detect_constraints(markets)
            OUTPUT_PATH.parent.mkdir(parents=True, exist_ok=True)
            detector.save_constraints(OUTPUT_PATH)
            log.info(f'[iter {iteration}] Wrote {len(constraints)} constraints')
            write_status('running', len(constraints))
        except Exception as e:
            log.error(f'[iter {iteration}] Error: {e}', exc_info=True)
            write_status('error', 0, str(e))
        log.debug(f'[iter {iteration}] Sleeping {rebuild_interval}s')
        time.sleep(rebuild_interval)

if __name__ == '__main__':
    main()
