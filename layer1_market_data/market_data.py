"""
Layer 1: Market Data Collection
The Eyes of the trading system - continuously pulls and stores market data
"""

import asyncio
import json
import logging
from abc import ABC, abstractmethod
from dataclasses import dataclass, asdict
from datetime import datetime, timezone
from typing import Dict, List, Optional, Any
from pathlib import Path
import aiohttp


@dataclass
class MarketData:
    """Represents a single market data point"""
    market_id: str
    market_name: str
    question: str
    outcome_prices: Dict[str, float]  # outcome -> price
    volume_24h: float
    liquidity: float
    end_date: datetime
    categories: List[str]
    metadata: Dict[str, Any]
    timestamp: datetime
    source: str  # polymarket, kalshi, etc
    
    def to_dict(self) -> Dict:
        """Convert to dictionary for storage"""
        data = asdict(self)
        data['end_date'] = self.end_date.isoformat()
        data['timestamp'] = self.timestamp.isoformat()
        return data
    
    @classmethod
    def from_dict(cls, data: Dict) -> 'MarketData':
        """Create from dictionary"""
        data['end_date'] = datetime.fromisoformat(data['end_date'])
        data['timestamp'] = datetime.fromisoformat(data['timestamp'])
        return cls(**data)


class MarketDataCollector(ABC):
    """Base class for market data collectors"""
    
    def __init__(self, config: Dict, data_dir: Path):
        self.config = config
        self.data_dir = data_dir
        self.data_dir.mkdir(parents=True, exist_ok=True)
        self.logger = logging.getLogger(f"{self.__class__.__name__}")
        self.session: Optional[aiohttp.ClientSession] = None
        self.running = False
        
    async def start(self):
        """Start the data collection loop"""
        self.session = aiohttp.ClientSession()
        self.running = True
        self.logger.info(f"{self.__class__.__name__} started")
        
        while self.running:
            try:
                await self.collect_and_store()
                await asyncio.sleep(self.config.get('poll_interval', 30))
            except Exception as e:
                self.logger.error(f"Error in collection loop: {e}", exc_info=True)
                await asyncio.sleep(5)  # Brief pause before retry
    
    async def stop(self):
        """Stop the data collection"""
        self.running = False
        if self.session:
            await self.session.close()
        self.logger.info(f"{self.__class__.__name__} stopped")
    
    async def collect_and_store(self):
        """Collect current market data and store it"""
        markets = await self.fetch_markets()
        await self.store_markets(markets)
        self.logger.debug(f"Collected and stored {len(markets)} markets")
    
    @abstractmethod
    async def fetch_markets(self) -> List[MarketData]:
        """Fetch current market data from the exchange
        Must be implemented by subclasses"""
        pass
    
    async def store_markets(self, markets: List[MarketData]):
        """Store market data to disk"""
        timestamp = datetime.now(timezone.utc)
        filename = f"{timestamp.strftime('%Y%m%d_%H%M%S')}.json"
        filepath = self.data_dir / filename
        
        data = {
            'timestamp': timestamp.isoformat(),
            'source': self.__class__.__name__,
            'market_count': len(markets),
            'markets': [m.to_dict() for m in markets]
        }
        
        with open(filepath, 'w') as f:
            json.dump(data, f, indent=2)
        
        # Also maintain a "latest.json" for quick access
        latest_path = self.data_dir / 'latest.json'
        with open(latest_path, 'w') as f:
            json.dump(data, f, indent=2)
    
    def load_latest_markets(self) -> List[MarketData]:
        """Load the most recent market data"""
        latest_path = self.data_dir / 'latest.json'
        if not latest_path.exists():
            return []
        
        with open(latest_path, 'r') as f:
            data = json.load(f)
        
        return [MarketData.from_dict(m) for m in data['markets']]


class PolymarketCollector(MarketDataCollector):
    """Polymarket-specific data collector"""
    
    def __init__(self, config: Dict, data_dir: Path):
        super().__init__(config, data_dir)
        self.api_url = config.get('polymarket', {}).get('api_url', 
                                                         'https://gamma-api.polymarket.com')
    
    async def fetch_markets(self) -> List[MarketData]:
        """Fetch markets from Polymarket API with pagination"""
        try:
            all_markets = []
            url = f"{self.api_url}/markets"
            
            # Paginate until ALL markets collected (no hard cap)
            page = 0
            while True:
                params = {
                    'active': 'true',
                    'closed': 'false',
                    'limit': 100,
                    'offset': page * 100
                }
                
                async with self.session.get(url, params=params) as response:
                    if response.status != 200:
                        self.logger.error(f"API returned status {response.status} on page {page}")
                        break
                    
                    data = await response.json()
                    if not data:  # No more results
                        break
                    
                    all_markets.extend(data)
                    
                    # If we got fewer than limit, we've reached the end
                    if len(data) < 100:
                        break
                    
                    page += 1
            
            self.logger.info(f"Fetched {len(all_markets)} markets across {page + 1} pages")
            return self._parse_polymarket_data(all_markets)
        
        except Exception as e:
            self.logger.error(f"Error fetching Polymarket data: {e}", exc_info=True)
            return []
    
    def _parse_polymarket_data(self, raw_data: List[Dict]) -> List[MarketData]:
        self.logger.info(f"_parse_polymarket_data received {len(raw_data)} raw markets")
        """Parse Polymarket API response into MarketData objects"""
        markets = []
        
        for market in raw_data:
            try:
                # Parse outcome prices (both are JSON strings)
                import json as json_module
                outcome_prices = {}
                
                outcomes = market.get('outcomes', [])
                prices = market.get('outcomePrices', [])
                
                # Parse both as JSON if they're strings
                if isinstance(outcomes, str):
                    try:
                        outcomes = json_module.loads(outcomes)
                    except:
                        outcomes = []
                
                if isinstance(prices, str):
                    try:
                        prices = json_module.loads(prices)
                    except:
                        prices = []
                
                for i, outcome in enumerate(outcomes):
                    if i < len(prices):
                        outcome_prices[outcome] = float(prices[i])
                
                # Parse end date
                end_date_str = market.get('endDate') or market.get('end_date')
                end_date = datetime.fromisoformat(end_date_str.replace('Z', '+00:00')) \
                           if end_date_str else datetime.now(timezone.utc)
                
                market_data = MarketData(
                    market_id=market['id'],
                    market_name=market.get('question', 'Unknown'),
                    question=market.get('question', 'Unknown'),
                    outcome_prices=outcome_prices,
                    volume_24h=float(market.get('volume_24h') or market.get('volume24hr') or 0),
                    liquidity=float(market.get('liquidity', 0)),
                    end_date=end_date,
                    categories=market.get('tags', []),
                    metadata={
                        'conditionId': market.get('conditionId', ''),
                        'questionID': market.get('questionID', ''),
                        'negRisk': market.get('negRisk', False),
                        'negRiskMarketID': market.get('negRiskMarketID', ''),
                        'groupItemTitle': market.get('groupItemTitle', ''),
                        'slug': market.get('slug', ''),
                        'clobTokenIds': market.get('clobTokenIds', ''),
                        'enableOrderBook': market.get('enableOrderBook', False),
                        'acceptingOrders': market.get('acceptingOrders', False),
                    },
                    timestamp=datetime.now(timezone.utc),
                    source='polymarket'
                )
                
                markets.append(market_data)
            
            except Exception as e:
                self.logger.warning(f"Error parsing market {market.get('id')}: {e}")
                continue
        
        return markets


class MarketDataManager:
    """Manages multiple market data collectors"""
    
    def __init__(self, config: Dict, workspace_root: Path):
        self.config = config
        self.workspace_root = Path(workspace_root)
        self.data_dir = self.workspace_root / 'layer1_market_data' / 'data'
        self.logger = logging.getLogger('MarketDataManager')
        self.collectors = []
        
        # Initialize collectors based on config
        enabled_markets = config.get('market_data', {}).get('enabled_markets', [])
        
        if 'polymarket' in enabled_markets:
            collector = PolymarketCollector(
                config.get('market_data', {}),
                self.data_dir / 'polymarket'
            )
            self.collectors.append(collector)
            self.logger.info("Initialized Polymarket collector")
    
    async def start_all(self):
        """Start all collectors"""
        tasks = [collector.start() for collector in self.collectors]
        await asyncio.gather(*tasks)
    
    async def stop_all(self):
        """Stop all collectors"""
        tasks = [collector.stop() for collector in self.collectors]
        await asyncio.gather(*tasks)
    
    def get_all_latest_markets(self) -> List[MarketData]:
        """Get latest data from all collectors"""
        all_markets = []
        for collector in self.collectors:
            markets = collector.load_latest_markets()
            all_markets.extend(markets)
        return all_markets


if __name__ == '__main__':
    # Example usage
    import yaml
    
    logging.basicConfig(level=logging.INFO)
    
    # Load config
    config_path = Path('../config/config.yaml')
    with open(config_path) as f:
        config = yaml.safe_load(f)
    
    # Create and run manager
    manager = MarketDataManager(config, Path('../'))
    
    async def main():
        try:
            await manager.start_all()
        except KeyboardInterrupt:
            await manager.stop_all()
    
    asyncio.run(main())
