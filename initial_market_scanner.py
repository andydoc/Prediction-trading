"""Initial Market Scanner — run once at startup to populate latest_markets.json.

After this completes, the Trading Engine takes over with WebSocket-based
market discovery and price monitoring. No polling loop needed.
"""
import asyncio, json, logging, sys, yaml
from pathlib import Path
from datetime import datetime
from zoneinfo import ZoneInfo

sys.path.append(str(Path(__file__).parent))
from layer1_market_data.market_data import MarketDataManager

WORKSPACE = Path('/home/andydoc/prediction-trader')
CONFIG_PATH = WORKSPACE / 'config' / 'config.yaml'
OUTPUT_PATH = WORKSPACE / 'data' / 'latest_markets.json'
STATUS_PATH = WORKSPACE / 'data' / 'layer1_status.json'

logging.basicConfig(level=logging.DEBUG,
    format='%(asctime)s - [SCANNER] %(levelname)s - %(message)s',
    handlers=[
        logging.FileHandler(str(
            WORKSPACE / 'logs' / f'scanner_{datetime.now().strftime("%Y%m%d")}.log'
        )),
        logging.StreamHandler(),
    ])
log = logging.getLogger('scanner')


def write_status(status, count=0, error=None):
    STATUS_PATH.parent.mkdir(parents=True, exist_ok=True)
    STATUS_PATH.write_text(json.dumps({
        'status': status, 'market_count': count, 'error': error,
        'timestamp': datetime.now(ZoneInfo('Europe/London')).isoformat()
    }))


async def scan_once():
    """Fetch all markets from Polymarket Gamma API and write latest_markets.json."""
    with open(CONFIG_PATH) as f:
        config = yaml.safe_load(f)

    manager = MarketDataManager(config, WORKSPACE)
    log.info('Initial market scan starting...')
    write_status('scanning')

    # Start collectors (triggers initial fetch)
    await manager.start_all()

    # Give the async fetch time to complete — poll until we have data
    for attempt in range(30):  # max 30 attempts × 2s = 60s
        await asyncio.sleep(2)
        markets = manager.get_all_latest_markets()
        if markets and len(markets) > 1000:  # expect 30k+, but 1k means fetch is working
            break
        log.debug(f'  attempt {attempt+1}: {len(markets) if markets else 0} markets so far...')

    markets = manager.get_all_latest_markets()
    await manager.stop_all()

    if not markets:
        log.error('No markets fetched — check Gamma API connectivity')
        write_status('error', 0, 'No markets fetched')
        sys.exit(1)

    # Write output
    OUTPUT_PATH.parent.mkdir(parents=True, exist_ok=True)
    market_dicts = [m.to_dict() if hasattr(m, 'to_dict') else m for m in markets]
    OUTPUT_PATH.write_text(json.dumps({
        'markets': market_dicts,
        'count': len(market_dicts),
        'timestamp': datetime.now(ZoneInfo('Europe/London')).isoformat(),
    }))

    log.info(f'Scan complete: {len(markets)} markets written to {OUTPUT_PATH.name}')
    write_status('done', len(markets))


if __name__ == '__main__':
    asyncio.run(scan_once())
    # Force exit — async manager may have dangling tasks that prevent clean shutdown
    import os
    os._exit(0)
