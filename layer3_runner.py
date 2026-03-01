"""Layer 3 Runner - Arbitrage Math (with incremental opportunity writing)"""
import json, logging, sys, time, yaml, signal
from pathlib import Path
from datetime import datetime
from zoneinfo import ZoneInfo

sys.path.append(str(Path(__file__).parent))
from layer3_arbitrage_math.arbitrage_engine import ArbitrageMathEngine
from layer2_constraint_detection.constraint_detector import ConstraintDetector
from layer1_market_data.market_data import MarketData

WORKSPACE        = Path('/home/andydoc/prediction-trader')
CONFIG_PATH      = WORKSPACE / 'config' / 'config.yaml'
CONSTRAINTS_PATH = WORKSPACE / 'layer2_constraint_detection' / 'data' / 'latest_constraints.json'
MARKETS_PATH     = WORKSPACE / 'data' / 'latest_markets.json'
OUTPUT_PATH      = WORKSPACE / 'layer3_arbitrage_math' / 'data' / 'latest_opportunities.json'
STATUS_PATH      = WORKSPACE / 'data' / 'layer3_status.json'

logging.basicConfig(level=logging.INFO,
    format='%(asctime)s - [L3] %(levelname)s - %(message)s',
    handlers=[logging.FileHandler(str(WORKSPACE / 'logs' / f'layer3_{datetime.now().strftime("%Y%m%d")}.log')),
              logging.StreamHandler()])
log = logging.getLogger('layer3')

def write_status(status, scanned=0, found=0, error=None):
    STATUS_PATH.parent.mkdir(parents=True, exist_ok=True)
    STATUS_PATH.write_text(json.dumps({
        'status': status, 'constraints_scanned': scanned,
        'opportunities_found': found, 'error': error,
        'timestamp': datetime.now(ZoneInfo('Europe/London')).isoformat()
    }))

def write_opportunities(opps):
    """Write opportunities to JSON immediately so Layer 4 can see them."""
    OUTPUT_PATH.parent.mkdir(parents=True, exist_ok=True)
    OUTPUT_PATH.write_text(json.dumps({
        'opportunities': [o.to_dict() for o in opps],
        'count': len(opps),
        'timestamp': datetime.now(ZoneInfo('Europe/London')).isoformat()
    }, indent=2))

class ConstraintTimeout(Exception):
    pass

def _timeout_handler(signum, frame):
    raise ConstraintTimeout("Constraint check timed out")

def main():
    with open(CONFIG_PATH) as f:
        config = yaml.safe_load(f)
    scan_sleep = 5
    engine = ArbitrageMathEngine(config, WORKSPACE)
    detector = ConstraintDetector(config, WORKSPACE)
    log.info('Layer 3 started (incremental mode)')
    write_status('starting')
    iteration = 0

    while True:
        iteration += 1
        try:
            if not CONSTRAINTS_PATH.exists() or not MARKETS_PATH.exists():
                log.warning(f'[iter {iteration}] Waiting for upstream layers...')
                write_status('waiting')
                time.sleep(30)
                continue

            market_data = json.loads(MARKETS_PATH.read_text())
            markets = [MarketData.from_dict(m) for m in market_data.get('markets', [])]
            constraints = detector.load_constraints(CONSTRAINTS_PATH)
            total = len(constraints)
            log.info(f'[iter {iteration}] Scanning {total} constraints...')
            write_status('scanning', total)

            # Scan constraints one by one, write incrementally
            opportunities = []
            skipped = 0
            errors = 0
            t0 = time.time()

            for i, constraint in enumerate(constraints, 1):
                try:
                    # Per-constraint timeout: 5 seconds max
                    signal.signal(signal.SIGALRM, _timeout_handler)
                    signal.alarm(5)

                    opp = engine._check_constraint_for_arbitrage(constraint, markets)

                    signal.alarm(0)  # cancel alarm

                    if opp:
                        opportunities.append(opp)
                        write_opportunities(opportunities)  # write immediately
                        log.debug(
                            f"  [{i}/{total}] ARBITRAGE  "
                            f"type={constraint.relationship_type}  "
                            f"markets={len(constraint.market_ids)}  "
                            f"profit={opp.net_profit:.4f}  "
                            f"method={opp.metadata.get('method','?')}")
                    elif i <= 5 or i % 20 == 0:
                        log.debug(
                            f"  [{i}/{total}] No arb: "
                            f"type={constraint.relationship_type}  "
                            f"markets={len(constraint.market_ids)}")

                except ConstraintTimeout:
                    signal.alarm(0)
                    skipped += 1
                    log.warning(f"  [{i}/{total}] TIMEOUT: constraint {constraint.constraint_id} "
                               f"({len(constraint.market_ids)} markets) - skipped")
                except Exception as e:
                    signal.alarm(0)
                    errors += 1
                    log.error(f"  [{i}/{total}] Error: {e}")

            elapsed = time.time() - t0
            log.info(f'[iter {iteration}] SCAN COMPLETE in {elapsed:.1f}s: '
                     f'{len(opportunities)} opps, {skipped} timeouts, {errors} errors')

            # Final write (even if empty)
            write_opportunities(opportunities)
            engine.opportunities = opportunities

            write_status('idle', total, len(opportunities))

        except Exception as e:
            log.error(f'[iter {iteration}] Error: {e}', exc_info=True)
            write_status('error', 0, 0, str(e))

        time.sleep(scan_sleep)

if __name__ == '__main__':
    main()
