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
WS_PRICES_PATH   = WORKSPACE / 'data' / 'ws_prices.json'       # Phase 6c: WS price bridge from L4

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

def overlay_ws_prices(markets: list, max_age_secs: float = 30.0) -> int:
    """Overlay live WS prices onto MarketData objects.
    Reads ws_prices.json written by L4's WS manager.
    Returns count of markets updated. Skips stale data (>max_age_secs)."""
    if not WS_PRICES_PATH.exists():
        return 0
    try:
        data = json.loads(WS_PRICES_PATH.read_text())
        prices = data.get('prices', {})
        exported_at = data.get('exported_at', 0)
        # Skip if bridge file itself is stale (L4 stopped writing)
        import time as _t
        if _t.time() - exported_at > 60:
            return 0
        updated = 0
        for m in markets:
            mid = str(m.market_id)
            if mid not in prices:
                continue
            mp = prices[mid]
            ts = mp.get('ts', 0)
            if ts and (_t.time() - ts) > max_age_secs:
                continue  # WS data too old for this market
            yes_p = mp.get('Yes')
            if yes_p is not None and yes_p > 0:
                m.outcome_prices['Yes'] = float(yes_p)
                m.outcome_prices['No'] = round(1.0 - float(yes_p), 6)
                updated += 1
        return updated
    except Exception:
        return 0

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

            # Overlay live WS prices (Phase 6c) — replaces 30s-stale L1 prices
            ws_updated = overlay_ws_prices(markets)

            constraints = detector.load_constraints(CONSTRAINTS_PATH)
            total = len(constraints)
            log.info(f'[iter {iteration}] Scanning {total} constraints... (WS prices: {ws_updated} markets updated)')
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
