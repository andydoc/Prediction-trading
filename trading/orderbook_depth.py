"""
Order Book Depth Analysis Module
Phase 5a: Fetches CLOB order books, calculates effective depth at 80% haircut.
Used by L4 to validate liquidity before entering/replacing positions.

Usage:
    depth_checker = OrderBookDepthChecker(clob_host="https://clob.polymarket.com")
    result = await depth_checker.check_opportunity_depth(
        opportunity=opp_dict,
        market_lookup=market_lookup,
        max_trade_size=1000.0
    )
    if result['tradeable']:
        effective_size = result['effective_trade_size']
"""

import asyncio
import json
import logging
from typing import Dict, List, Optional, Tuple, Any
from dataclasses import dataclass, field, asdict
from datetime import datetime, timezone

import aiohttp

log = logging.getLogger('orderbook_depth')

# --- Configuration ---
DEPTH_HAIRCUT = 0.80          # Use 80% of reported book depth
MIN_DEPTH_FLOOR = 5.0         # Skip if effective depth < $5 on any leg
BOOK_FETCH_TIMEOUT = 10       # Seconds per book fetch
MAX_CONCURRENT_FETCHES = 5    # Parallel book fetches


@dataclass
class LegDepth:
    """Depth analysis for one leg of an arbitrage"""
    market_id: str
    token_id: str
    side: str                     # 'buy' or 'sell'
    raw_depth_usd: float          # Total $ available on relevant side
    effective_depth_usd: float    # After 80% haircut
    num_price_levels: int         # Number of price levels in book
    best_price: float             # Best available price
    worst_price_in_depth: float   # Worst price within usable depth
    depth_levels: List[Dict]      # [{price, size, cumulative_usd}]


@dataclass
class OpportunityDepthResult:
    """Depth analysis for an entire arbitrage opportunity"""
    opportunity_id: str
    tradeable: bool               # All legs meet minimum depth?
    effective_trade_size: float   # Min effective depth across all legs
    requested_trade_size: float   # What we wanted to trade
    depth_limited: bool           # Was size reduced due to depth?
    min_leg_depth: float          # Smallest leg depth (bottleneck)
    bottleneck_market_id: str     # Which leg is the bottleneck
    legs: List[LegDepth] = field(default_factory=list)
    error: Optional[str] = None
    fetch_time_ms: float = 0.0

    def to_dict(self) -> Dict:
        return {
            'opportunity_id': self.opportunity_id,
            'tradeable': self.tradeable,
            'effective_trade_size': round(self.effective_trade_size, 4),
            'requested_trade_size': round(self.requested_trade_size, 4),
            'depth_limited': self.depth_limited,
            'min_leg_depth': round(self.min_leg_depth, 4),
            'bottleneck_market_id': self.bottleneck_market_id,
            'legs': [asdict(l) for l in self.legs],
            'error': self.error,
            'fetch_time_ms': round(self.fetch_time_ms, 1),
        }


def get_token_id_for_market(market_id: str, market_lookup: Dict, strategy: str) -> Optional[str]:
    """
    Extract the correct CLOB token_id for a market given the strategy.
    - mutex_buy_all: buying YES → asks on YES token (index 0)
    - mutex_sell_all: buying NO  → asks on NO token (index 1)
    """
    md = market_lookup.get(str(market_id))
    if not md:
        log.warning(f"Market {market_id} not in lookup")
        return None

    clob_ids_raw = md.metadata.get('clobTokenIds', '[]')
    try:
        clob_ids = json.loads(clob_ids_raw) if isinstance(clob_ids_raw, str) else clob_ids_raw
    except (json.JSONDecodeError, TypeError):
        log.warning(f"Bad clobTokenIds for {market_id}: {clob_ids_raw}")
        return None

    if not clob_ids or len(clob_ids) < 2:
        log.warning(f"Market {market_id} has <2 clob tokens")
        return None

    # YES token = index 0, NO token = index 1
    if 'sell' in strategy.lower():
        return clob_ids[1]   # NO token for sell arb
    else:
        return clob_ids[0]   # YES token for buy arb


class OrderBookDepthChecker:
    """Fetches order books from Polymarket CLOB and calculates effective depth."""

    def __init__(self, clob_host: str = "https://clob.polymarket.com",
                 haircut: float = DEPTH_HAIRCUT,
                 min_depth_floor: float = MIN_DEPTH_FLOOR):
        self.clob_host = clob_host.rstrip('/')
        self.haircut = haircut
        self.min_depth_floor = min_depth_floor

    async def fetch_order_book(self, session: aiohttp.ClientSession,
                                token_id: str) -> Optional[Dict]:
        """Fetch order book for a single token from CLOB API."""
        url = f"{self.clob_host}/book"
        params = {"token_id": token_id}
        try:
            async with session.get(url, params=params,
                                   timeout=aiohttp.ClientTimeout(total=BOOK_FETCH_TIMEOUT)) as resp:
                if resp.status == 200:
                    return await resp.json()
                else:
                    text = await resp.text()
                    log.warning(f"Book fetch {resp.status} for {token_id[:20]}...: {text[:100]}")
                    return None
        except Exception as e:
            log.warning(f"Book fetch error for {token_id[:20]}...: {e}")
            return None

    def calculate_ask_depth(self, book: Dict, max_usd: float = 100000.0) -> LegDepth:
        """
        Calculate depth on the ASK side (we're buying).
        Returns cumulative USD available at each price level.
        
        For buy arb: we buy YES tokens, so we're taking asks on YES book.
        For sell arb: we buy NO tokens, so we're taking asks on NO book.
        """
        asks = book.get('asks', [])
        if not asks:
            return LegDepth(
                market_id=book.get('market', ''),
                token_id=book.get('asset_id', ''),
                side='buy',
                raw_depth_usd=0.0,
                effective_depth_usd=0.0,
                num_price_levels=0,
                best_price=0.0,
                worst_price_in_depth=0.0,
                depth_levels=[],
            )

        # Sort asks by price ascending (best asks first)
        sorted_asks = sorted(asks, key=lambda x: float(x.get('price', '999')))

        cumulative_usd = 0.0
        depth_levels = []
        worst_price = 0.0

        for level in sorted_asks:
            price = float(level.get('price', '0'))
            size = float(level.get('size', '0'))
            level_usd = price * size  # Cost to buy these shares
            cumulative_usd += level_usd
            worst_price = price
            depth_levels.append({
                'price': price,
                'size': size,
                'level_usd': round(level_usd, 4),
                'cumulative_usd': round(cumulative_usd, 4),
            })
            if cumulative_usd >= max_usd:
                break

        raw_depth = cumulative_usd
        effective_depth = raw_depth * self.haircut

        return LegDepth(
            market_id=book.get('market', ''),
            token_id=book.get('asset_id', ''),
            side='buy',
            raw_depth_usd=round(raw_depth, 4),
            effective_depth_usd=round(effective_depth, 4),
            num_price_levels=len(depth_levels),
            best_price=float(sorted_asks[0].get('price', '0')) if sorted_asks else 0.0,
            worst_price_in_depth=worst_price,
            depth_levels=depth_levels,
        )

    async def check_opportunity_depth(
        self,
        opportunity: Dict,
        market_lookup: Dict,
        max_trade_size: float,
    ) -> OpportunityDepthResult:
        """
        Check order book depth for all legs of an arbitrage opportunity.
        
        Returns OpportunityDepthResult with:
        - tradeable: True if all legs have >= min_depth_floor
        - effective_trade_size: min(max_trade_size, min leg depth)
        - Per-leg depth details
        """
        opp_id = opportunity.get('opportunity_id', '?')
        market_ids = opportunity.get('market_ids', [])
        strategy = opportunity.get('metadata', {}).get('method', 'mutex_buy_all')
        start = asyncio.get_event_loop().time()

        # Resolve token IDs
        token_map = {}  # market_id -> token_id
        for mid in market_ids:
            tid = get_token_id_for_market(mid, market_lookup, strategy)
            if tid:
                token_map[mid] = tid
            else:
                return OpportunityDepthResult(
                    opportunity_id=opp_id, tradeable=False,
                    effective_trade_size=0, requested_trade_size=max_trade_size,
                    depth_limited=True, min_leg_depth=0,
                    bottleneck_market_id=mid,
                    error=f"No token_id for market {mid}",
                )

        # Fetch all order books in parallel
        legs = []
        async with aiohttp.ClientSession() as session:
            sem = asyncio.Semaphore(MAX_CONCURRENT_FETCHES)

            async def fetch_with_sem(mid, tid):
                async with sem:
                    book = await self.fetch_order_book(session, tid)
                    return mid, tid, book

            tasks = [fetch_with_sem(mid, tid) for mid, tid in token_map.items()]
            results = await asyncio.gather(*tasks, return_exceptions=True)

        # Process results
        for r in results:
            if isinstance(r, Exception):
                log.warning(f"Book fetch exception for {opp_id}: {r}")
                continue
            mid, tid, book = r
            if book is None:
                legs.append(LegDepth(
                    market_id=mid, token_id=tid, side='buy',
                    raw_depth_usd=0, effective_depth_usd=0,
                    num_price_levels=0, best_price=0,
                    worst_price_in_depth=0, depth_levels=[],
                ))
                continue
            leg = self.calculate_ask_depth(book, max_usd=max_trade_size * 2)
            leg.market_id = mid
            leg.token_id = tid
            legs.append(leg)

        # Find bottleneck
        elapsed = (asyncio.get_event_loop().time() - start) * 1000

        if not legs:
            return OpportunityDepthResult(
                opportunity_id=opp_id, tradeable=False,
                effective_trade_size=0, requested_trade_size=max_trade_size,
                depth_limited=True, min_leg_depth=0,
                bottleneck_market_id=market_ids[0] if market_ids else '',
                legs=legs, error="No book data retrieved",
                fetch_time_ms=elapsed,
            )

        min_leg = min(legs, key=lambda l: l.effective_depth_usd)
        min_depth = min_leg.effective_depth_usd
        tradeable = min_depth >= self.min_depth_floor
        effective_size = min(max_trade_size, min_depth) if tradeable else 0.0
        depth_limited = effective_size < max_trade_size

        result = OpportunityDepthResult(
            opportunity_id=opp_id,
            tradeable=tradeable,
            effective_trade_size=round(effective_size, 4),
            requested_trade_size=round(max_trade_size, 4),
            depth_limited=depth_limited,
            min_leg_depth=round(min_depth, 4),
            bottleneck_market_id=min_leg.market_id,
            legs=legs,
            fetch_time_ms=elapsed,
        )

        log.debug(
            f"Depth check {opp_id[:30]}: "
            f"tradeable={tradeable} "
            f"effective=${effective_size:.2f} "
            f"(requested=${max_trade_size:.2f}, "
            f"min_leg=${min_depth:.2f} @ {min_leg.market_id}) "
            f"[{elapsed:.0f}ms]"
        )
        return result


# --- Convenience for L4 integration ---

async def check_depth_for_opportunities(
    opportunities: List[Dict],
    market_lookup: Dict,
    max_trade_size: float,
    clob_host: str = "https://clob.polymarket.com",
) -> Dict[str, OpportunityDepthResult]:
    """
    Batch check depth for multiple opportunities.
    Returns dict of opportunity_id -> OpportunityDepthResult.
    """
    checker = OrderBookDepthChecker(clob_host=clob_host)
    results = {}
    for opp in opportunities:
        oid = opp.get('opportunity_id', '?')
        result = await checker.check_opportunity_depth(opp, market_lookup, max_trade_size)
        results[oid] = result
    return results


if __name__ == '__main__':
    """Quick test: fetch depth for the top opportunity."""
    import sys
    sys.path.append(str(__import__('pathlib').Path(__file__).parent))
    from market_data.market_data import MarketData

    async def main():
        logging.basicConfig(level=logging.DEBUG)
        workspace = __import__('pathlib').Path(__file__).parent

        # Load markets
        markets_path = workspace / 'data' / 'latest_markets.json'
        data = json.loads(markets_path.read_text())
        mlist = data.get('markets', data) if isinstance(data, dict) else data
        markets = [MarketData.from_dict(m) for m in mlist]
        market_lookup = {str(m.market_id): m for m in markets}
        print(f"Loaded {len(markets)} markets")

        # Load opportunities
        opp_path = workspace / 'arbitrage_math' / 'data' / 'latest_opportunities.json'
        opps = json.loads(opp_path.read_text()).get('opportunities', [])
        print(f"Loaded {len(opps)} opportunities")

        if not opps:
            print("No opportunities to check")
            return

        # Check top 3
        checker = OrderBookDepthChecker()
        for opp in opps[:3]:
            oid = opp.get('opportunity_id', '?')
            names = opp.get('market_names', [])
            print(f"\n{'='*60}")
            print(f"Opp: {oid[:50]}")
            print(f"Markets: {[n[:40] for n in names]}")

            result = await checker.check_opportunity_depth(
                opp, market_lookup, max_trade_size=100.0
            )
            print(f"Tradeable: {result.tradeable}")
            print(f"Effective size: ${result.effective_trade_size:.2f}")
            print(f"Requested: ${result.requested_trade_size:.2f}")
            print(f"Depth limited: {result.depth_limited}")
            print(f"Min leg depth: ${result.min_leg_depth:.2f}")
            print(f"Bottleneck: {result.bottleneck_market_id}")
            print(f"Fetch time: {result.fetch_time_ms:.0f}ms")
            for leg in result.legs:
                print(f"  Leg {leg.market_id}: "
                      f"raw=${leg.raw_depth_usd:.2f} "
                      f"eff=${leg.effective_depth_usd:.2f} "
                      f"levels={leg.num_price_levels} "
                      f"best={leg.best_price:.4f}")

    asyncio.run(main())
