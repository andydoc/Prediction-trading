"""Layer 1 Runner - Market Data Collection"""
import asyncio, json, logging, sys, yaml, signal
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
    format='%(asctime)s - [L1] %(levelname)s - %(message)s',
    handlers=[logging.FileHandler(str(WORKSPACE / 'logs' / f'layer1_{datetime.now().strftime("%Y%m%d")}.log')), logging.StreamHandler()])
log = logging.getLogger('layer1')

running = True

def handle_shutdown(signum, frame):
    global running
    log.info(f'Received signal {signum}, shutting down...')
    running = False

def write_status(status, count=0, error=None):
    STATUS_PATH.write_text(json.dumps({'status': status, 'market_count': count, 'error': error, 'timestamp': datetime.now(ZoneInfo('Europe/London')).isoformat()}))

async def main():
    global running
    signal.signal(signal.SIGTERM, handle_shutdown)
    signal.signal(signal.SIGINT, handle_shutdown)
    
    with open(CONFIG_PATH) as f:
        config = yaml.safe_load(f)
    poll_interval = config.get('market_data', {}).get('poll_interval', 30)
    manager = MarketDataManager(config, WORKSPACE)
    log.info(f'Layer 1 started, poll_interval={poll_interval}s')
    write_status('starting')
    asyncio.create_task(manager.start_all())
    iteration = 0
    
    try:
        while running:
            iteration += 1
            try:
                markets = manager.get_all_latest_markets()
                if markets:
                    OUTPUT_PATH.parent.mkdir(parents=True, exist_ok=True)
                    market_dicts = [m.to_dict() if hasattr(m, 'to_dict') else m for m in markets]
                    OUTPUT_PATH.write_text(json.dumps({'markets': market_dicts, 'count': len(market_dicts), 'timestamp': datetime.now(ZoneInfo('Europe/London')).isoformat(), 'iteration': iteration}))
                    log.info(f'[iter {iteration}] Wrote {len(markets)} markets')
                    write_status('running', len(markets))
                else:
                    log.warning(f'[iter {iteration}] No market data yet')
                    write_status('waiting', 0)
            except Exception as e:
                log.error(f'[iter {iteration}] Error: {e}', exc_info=True)
                write_status('error', 0, str(e))
            await asyncio.sleep(poll_interval)
    finally:
        log.info('Cleaning up...')
        await manager.stop_all()
        write_status('stopped')
        log.info('Layer 1 stopped')

if __name__ == '__main__':
    asyncio.run(main())
