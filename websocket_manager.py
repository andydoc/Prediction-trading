"""
WebSocket Manager — Real-time market data and trade confirmations via Polymarket WebSocket.

Provides:
  1. Market Channel — live orderbook snapshots, price_change, best_bid_ask, market_resolved
  2. User Channel — trade lifecycle (MATCHED→CONFIRMED), order placements/cancellations

Used by:
  - L3 (arbitrage_engine): live price feed for validated constraint groups
  - L4 (layer4_runner): live orderbook depth + fill confirmations + resolution detection
  - orderbook_depth.py: can query local book mirror instead of REST /book

Architecture:
  - Two persistent WebSocket connections (market + user), each in its own asyncio task
  - Local orderbook mirror (dict[asset_id] -> {bids, asks, timestamp})
  - Callback system for consumers (L3, L4)
  - Auto-reconnect with exponential backoff
  - PING heartbeat every 10s
  - Dynamic subscription: add/remove asset_ids without reconnecting

Config (config.yaml):
  websocket:
    enabled: true
    market_channel_url: wss://ws-subscriptions-clob.polymarket.com/ws/market
    user_channel_url: wss://ws-subscriptions-clob.polymarket.com/ws/user
    heartbeat_interval: 10
    reconnect_base_delay: 1.0
    reconnect_max_delay: 60.0
"""

import asyncio
import json
import logging
import time
from typing import Dict, List, Optional, Set, Callable, Any
from dataclasses import dataclass, field
from datetime import datetime, timezone

try:
    import websockets
    from websockets.exceptions import ConnectionClosed
except ImportError:
    websockets = None

log = logging.getLogger('websocket_manager')

# --- Defaults ---
MARKET_WS_URL = 'wss://ws-subscriptions-clob.polymarket.com/ws/market'
USER_WS_URL = 'wss://ws-subscriptions-clob.polymarket.com/ws/user'
HEARTBEAT_INTERVAL = 10     # seconds
RECONNECT_BASE = 1.0        # seconds
RECONNECT_MAX = 60.0        # seconds


@dataclass
class OrderBookLevel:
    price: float
    size: float


@dataclass
class LocalOrderBook:
    """Local mirror of a single asset's order book."""
    asset_id: str
    bids: List[OrderBookLevel] = field(default_factory=list)
    asks: List[OrderBookLevel] = field(default_factory=list)
    best_bid: float = 0.0
    best_ask: float = 0.0
    spread: float = 0.0
    last_trade_price: float = 0.0
    last_update: float = 0.0   # unix timestamp
    update_count: int = 0

    def total_ask_depth_usd(self, max_levels: int = 50) -> float:
        """Total USD available on ask side (we're buying)."""
        total = 0.0
        for level in self.asks[:max_levels]:
            total += level.price * level.size
        return total

    def total_bid_depth_usd(self, max_levels: int = 50) -> float:
        """Total USD available on bid side (we're selling)."""
        total = 0.0
        for level in self.bids[:max_levels]:
            total += level.price * level.size
        return total

    def effective_ask_depth_usd(self, haircut: float = 0.80) -> float:
        """Ask depth with phantom order haircut (matches orderbook_depth.py)."""
        return self.total_ask_depth_usd() * haircut


# --- Callback type aliases ---
# on_price_change(asset_id, best_bid, best_ask, timestamp)
PriceCallback = Callable[[str, float, float, float], None]
# on_book_update(asset_id, local_book)
BookCallback = Callable[[str, LocalOrderBook], None]
# on_trade_confirm(trade_event_dict)
TradeCallback = Callable[[Dict], None]
# on_market_resolved(market_condition_id, winning_asset_id)
ResolvedCallback = Callable[[str, str], None]


class WebSocketManager:
    """
    Manages two persistent WebSocket connections to Polymarket:
      1. Market channel — orderbook, price changes, resolution events
      2. User channel — trade and order lifecycle events
    
    Maintains a local orderbook mirror that L3/L4 can query synchronously.
    """

    def __init__(self, config: Dict = None, secrets: Dict = None):
        if websockets is None:
            raise ImportError("websockets package required: pip install websockets")

        cfg = (config or {}).get('websocket', {})
        self.market_url = cfg.get('market_channel_url', MARKET_WS_URL)
        self.user_url = cfg.get('user_channel_url', USER_WS_URL)
        self.heartbeat_interval = cfg.get('heartbeat_interval', HEARTBEAT_INTERVAL)
        self.reconnect_base = cfg.get('reconnect_base_delay', RECONNECT_BASE)
        self.reconnect_max = cfg.get('reconnect_max_delay', RECONNECT_MAX)
        self.enabled = cfg.get('enabled', True)

        # Auth for user channel (from secrets.yaml → polymarket.api_key etc.)
        poly_secrets = (secrets or {}).get('polymarket', {})
        self._user_auth = None
        if poly_secrets.get('api_key') and poly_secrets.get('api_secret'):
            self._user_auth = {
                'apiKey': poly_secrets['api_key'],
                'secret': poly_secrets['api_secret'],
                'passphrase': poly_secrets.get('api_passphrase', ''),
            }

        # --- State ---
        self._books: Dict[str, LocalOrderBook] = {}     # asset_id -> LocalOrderBook
        self._subscribed_assets: Set[str] = set()        # asset_ids on market channel
        self._subscribed_markets: Set[str] = set()        # condition_ids on user channel
        self._market_ws = None
        self._user_ws = None
        self._market_task: Optional[asyncio.Task] = None
        self._user_task: Optional[asyncio.Task] = None
        self._heartbeat_task_market: Optional[asyncio.Task] = None
        self._heartbeat_task_user: Optional[asyncio.Task] = None
        self._running = False

        # --- Callbacks ---
        self._on_price_change: List[PriceCallback] = []
        self._on_book_update: List[BookCallback] = []
        self._on_trade_confirm: List[TradeCallback] = []
        self._on_market_resolved: List[ResolvedCallback] = []

        # --- Stats ---
        self._stats = {
            'market_msgs': 0,
            'user_msgs': 0,
            'reconnects_market': 0,
            'reconnects_user': 0,
            'last_market_msg': 0.0,
            'last_user_msg': 0.0,
        }

    # --- Public: callback registration ---

    def on_price_change(self, cb: PriceCallback):
        self._on_price_change.append(cb)

    def on_book_update(self, cb: BookCallback):
        self._on_book_update.append(cb)

    def on_trade_confirm(self, cb: TradeCallback):
        self._on_trade_confirm.append(cb)

    def on_market_resolved(self, cb: ResolvedCallback):
        self._on_market_resolved.append(cb)

    # --- Public: query local book mirror ---

    def get_book(self, asset_id: str) -> Optional[LocalOrderBook]:
        """Get cached local orderbook for an asset. Returns None if not subscribed."""
        return self._books.get(asset_id)

    def get_best_prices(self, asset_id: str) -> Optional[Dict]:
        """Get best bid/ask/spread for an asset."""
        book = self._books.get(asset_id)
        if not book:
            return None
        return {
            'best_bid': book.best_bid,
            'best_ask': book.best_ask,
            'spread': book.spread,
            'last_trade_price': book.last_trade_price,
            'last_update': book.last_update,
        }

    def get_ask_depth_usd(self, asset_id: str, haircut: float = 0.80) -> float:
        """Get effective ask depth for an asset (with phantom order haircut).
        Returns 0.0 if not subscribed or no data."""
        book = self._books.get(asset_id)
        if not book:
            return 0.0
        return book.effective_ask_depth_usd(haircut)

    def get_all_subscribed_assets(self) -> Set[str]:
        return set(self._subscribed_assets)

    def get_stats(self) -> Dict:
        return dict(self._stats)

    # --- Public: subscription management ---

    async def subscribe_assets(self, asset_ids: List[str]):
        """Add asset_ids to market channel subscription (dynamic, no reconnect)."""
        new_ids = [aid for aid in asset_ids if aid not in self._subscribed_assets]
        if not new_ids:
            return
        self._subscribed_assets.update(new_ids)
        # Initialize empty books
        for aid in new_ids:
            if aid not in self._books:
                self._books[aid] = LocalOrderBook(asset_id=aid)
        # Send subscribe operation if connected
        if self._market_ws:
            try:
                msg = json.dumps({
                    'assets_ids': new_ids,
                    'operation': 'subscribe',
                    'custom_feature_enabled': True,
                })
                await self._market_ws.send(msg)
                log.info(f'WS: subscribed {len(new_ids)} new assets (total={len(self._subscribed_assets)})')
            except Exception as e:
                log.warning(f'WS: subscribe send error: {e}')

    async def unsubscribe_assets(self, asset_ids: List[str]):
        """Remove asset_ids from market channel subscription."""
        to_remove = [aid for aid in asset_ids if aid in self._subscribed_assets]
        if not to_remove:
            return
        self._subscribed_assets -= set(to_remove)
        for aid in to_remove:
            self._books.pop(aid, None)
        if self._market_ws:
            try:
                msg = json.dumps({
                    'assets_ids': to_remove,
                    'operation': 'unsubscribe',
                })
                await self._market_ws.send(msg)
                log.info(f'WS: unsubscribed {len(to_remove)} assets (total={len(self._subscribed_assets)})')
            except Exception as e:
                log.warning(f'WS: unsubscribe send error: {e}')

    async def subscribe_user_markets(self, condition_ids: List[str]):
        """Subscribe to user channel for trade/order events on given condition_ids."""
        new_ids = [cid for cid in condition_ids if cid not in self._subscribed_markets]
        if not new_ids:
            return
        self._subscribed_markets.update(new_ids)
        if self._user_ws and self._user_auth:
            try:
                msg = json.dumps({
                    'markets': new_ids,
                    'operation': 'subscribe',
                })
                await self._user_ws.send(msg)
                log.info(f'WS user: subscribed {len(new_ids)} markets')
            except Exception as e:
                log.warning(f'WS user: subscribe send error: {e}')

    # --- Lifecycle ---

    async def start(self):
        """Start both WebSocket connections as background tasks."""
        if not self.enabled:
            log.info('WS: disabled in config')
            return
        if self._running:
            return
        self._running = True
        self._market_task = asyncio.create_task(self._market_channel_loop())
        if self._user_auth:
            self._user_task = asyncio.create_task(self._user_channel_loop())
        else:
            log.info('WS user channel: no API credentials, skipping')
        log.info('WS: started')

    async def stop(self):
        """Gracefully close both connections."""
        self._running = False
        for task in [self._market_task, self._user_task,
                     self._heartbeat_task_market, self._heartbeat_task_user]:
            if task and not task.done():
                task.cancel()
                try:
                    await task
                except asyncio.CancelledError:
                    pass
        for ws in [self._market_ws, self._user_ws]:
            if ws:
                try:
                    await ws.close()
                except Exception:
                    pass
        self._market_ws = None
        self._user_ws = None
        log.info('WS: stopped')

    # --- Heartbeat ---

    async def _heartbeat_loop(self, ws, label: str):
        """Send PING every heartbeat_interval seconds."""
        try:
            while self._running and ws:
                await ws.send('PING')
                await asyncio.sleep(self.heartbeat_interval)
        except (ConnectionClosed, asyncio.CancelledError):
            pass
        except Exception as e:
            log.debug(f'WS heartbeat {label} error: {e}')

    # --- Market Channel ---

    async def _market_channel_loop(self):
        """Persistent market channel connection with auto-reconnect."""
        delay = self.reconnect_base
        while self._running:
            try:
                async with websockets.connect(self.market_url) as ws:
                    self._market_ws = ws
                    delay = self.reconnect_base  # reset on successful connect
                    self._stats['reconnects_market'] += 1
                    log.info(f'WS market: connected (reconnect #{self._stats["reconnects_market"]})')

                    # Send initial subscription
                    if self._subscribed_assets:
                        sub_msg = json.dumps({
                            'assets_ids': list(self._subscribed_assets),
                            'type': 'market',
                            'custom_feature_enabled': True,
                        })
                        await ws.send(sub_msg)
                        log.info(f'WS market: subscribed {len(self._subscribed_assets)} assets')

                    # Start heartbeat
                    self._heartbeat_task_market = asyncio.create_task(
                        self._heartbeat_loop(ws, 'market'))

                    # Message receive loop
                    async for raw_msg in ws:
                        if not self._running:
                            break
                        if raw_msg == 'PONG':
                            continue
                        self._stats['market_msgs'] += 1
                        self._stats['last_market_msg'] = time.time()
                        try:
                            data = json.loads(raw_msg)
                            # WS can send single dict or array of events
                            if isinstance(data, list):
                                for item in data:
                                    if isinstance(item, dict):
                                        await self._handle_market_message(item)
                            elif isinstance(data, dict):
                                await self._handle_market_message(data)
                            else:
                                log.debug(f'WS market: unexpected type {type(data).__name__}')
                        except json.JSONDecodeError:
                            log.debug(f'WS market: non-JSON msg: {raw_msg[:80]}')
                        except Exception as e:
                            log.warning(f'WS market msg handler error: {e}')

            except (ConnectionClosed, OSError) as e:
                log.warning(f'WS market: disconnected ({e}), reconnecting in {delay:.1f}s')
            except asyncio.CancelledError:
                break
            except Exception as e:
                log.error(f'WS market: unexpected error: {e}', exc_info=True)

            self._market_ws = None
            if self._heartbeat_task_market:
                self._heartbeat_task_market.cancel()
            if self._running:
                await asyncio.sleep(delay)
                delay = min(delay * 2, self.reconnect_max)

    # --- User Channel ---

    async def _user_channel_loop(self):
        """Persistent user channel connection with auto-reconnect."""
        delay = self.reconnect_base
        while self._running:
            try:
                async with websockets.connect(self.user_url) as ws:
                    self._user_ws = ws
                    delay = self.reconnect_base
                    self._stats['reconnects_user'] += 1
                    log.info(f'WS user: connected (reconnect #{self._stats["reconnects_user"]})')

                    # Send initial auth + subscription
                    sub_msg = {
                        'auth': self._user_auth,
                        'markets': list(self._subscribed_markets),
                        'type': 'user',
                    }
                    await ws.send(json.dumps(sub_msg))
                    log.info(f'WS user: subscribed {len(self._subscribed_markets)} markets')

                    # Heartbeat
                    self._heartbeat_task_user = asyncio.create_task(
                        self._heartbeat_loop(ws, 'user'))

                    async for raw_msg in ws:
                        if not self._running:
                            break
                        if raw_msg == 'PONG':
                            continue
                        self._stats['user_msgs'] += 1
                        self._stats['last_user_msg'] = time.time()
                        try:
                            data = json.loads(raw_msg)
                            await self._handle_user_message(data)
                        except json.JSONDecodeError:
                            log.debug(f'WS user: non-JSON msg: {raw_msg[:80]}')
                        except Exception as e:
                            log.warning(f'WS user msg handler error: {e}')

            except (ConnectionClosed, OSError) as e:
                log.warning(f'WS user: disconnected ({e}), reconnecting in {delay:.1f}s')
            except asyncio.CancelledError:
                break
            except Exception as e:
                log.error(f'WS user: unexpected error: {e}', exc_info=True)

            self._user_ws = None
            if self._heartbeat_task_user:
                self._heartbeat_task_user.cancel()
            if self._running:
                await asyncio.sleep(delay)
                delay = min(delay * 2, self.reconnect_max)

    # --- Message Handlers ---

    async def _handle_market_message(self, data: Dict):
        """Route market channel messages by event_type."""
        event_type = data.get('event_type', '')

        if event_type == 'book':
            self._handle_book(data)
        elif event_type == 'price_change':
            self._handle_price_change(data)
        elif event_type == 'best_bid_ask':
            self._handle_best_bid_ask(data)
        elif event_type == 'last_trade_price':
            self._handle_last_trade_price(data)
        elif event_type == 'market_resolved':
            await self._handle_market_resolved(data)
        elif event_type == 'tick_size_change':
            pass  # Logged but not acted on
        else:
            log.debug(f'WS market: unknown event_type={event_type}')

    def _handle_book(self, data: Dict):
        """Full orderbook snapshot — replace local book entirely."""
        asset_id = data.get('asset_id', '')
        if not asset_id:
            return
        book = self._books.get(asset_id)
        if not book:
            book = LocalOrderBook(asset_id=asset_id)
            self._books[asset_id] = book

        # Parse bids (sorted descending by price)
        raw_bids = data.get('bids', [])
        book.bids = sorted(
            [OrderBookLevel(float(b.get('price', 0)), float(b.get('size', 0))) for b in raw_bids],
            key=lambda l: l.price, reverse=True
        )
        # Parse asks (sorted ascending by price)
        raw_asks = data.get('asks', [])
        book.asks = sorted(
            [OrderBookLevel(float(a.get('price', 0)), float(a.get('size', 0))) for a in raw_asks],
            key=lambda l: l.price
        )
        book.best_bid = book.bids[0].price if book.bids else 0.0
        book.best_ask = book.asks[0].price if book.asks else 0.0
        book.spread = round(book.best_ask - book.best_bid, 6) if (book.best_ask and book.best_bid) else 0.0
        ts_raw = data.get('timestamp', '')
        book.last_update = float(ts_raw) / 1000 if ts_raw else time.time()
        book.update_count += 1

        # Fire callbacks
        for cb in self._on_book_update:
            try:
                cb(asset_id, book)
            except Exception as e:
                log.debug(f'Book callback error: {e}')

    def _handle_price_change(self, data: Dict):
        """Incremental price level update — update local book levels."""
        # New schema (post Sept 2025): data has asset_id + changes list
        # OR old list-of-changes at top level via 'price_changes' key
        asset_id = data.get('asset_id', '')
        changes = data.get('changes', data.get('price_changes', []))

        # Handle both formats: single-asset and multi-asset (via price_changes list)
        if not asset_id and isinstance(changes, list) and changes:
            # Multi-asset format: each change has its own asset_id
            for change in changes:
                ch_asset = change.get('asset_id', '')
                if ch_asset and ch_asset in self._books:
                    self._apply_price_level_change(ch_asset, change)
            return

        if asset_id and asset_id in self._books:
            for change in (changes if isinstance(changes, list) else [changes]):
                self._apply_price_level_change(asset_id, change)

    def _apply_price_level_change(self, asset_id: str, change: Dict):
        """Apply a single price level change to local book."""
        book = self._books.get(asset_id)
        if not book:
            return
        price = float(change.get('price', 0))
        size = float(change.get('size', 0))
        side = change.get('side', '').upper()

        if side == 'BUY':
            levels = book.bids
            # Update or insert/remove
            levels[:] = [l for l in levels if abs(l.price - price) > 1e-8]
            if size > 0:
                levels.append(OrderBookLevel(price, size))
                levels.sort(key=lambda l: l.price, reverse=True)
            book.best_bid = levels[0].price if levels else 0.0
        elif side == 'SELL':
            levels = book.asks
            levels[:] = [l for l in levels if abs(l.price - price) > 1e-8]
            if size > 0:
                levels.append(OrderBookLevel(price, size))
                levels.sort(key=lambda l: l.price)
            book.best_ask = levels[0].price if levels else 0.0

        book.spread = round(book.best_ask - book.best_bid, 6) if (book.best_ask and book.best_bid) else 0.0
        book.last_update = time.time()
        book.update_count += 1

        # Fire price callbacks
        for cb in self._on_price_change:
            try:
                cb(asset_id, book.best_bid, book.best_ask, book.last_update)
            except Exception as e:
                log.debug(f'Price callback error: {e}')

    def _handle_best_bid_ask(self, data: Dict):
        """Direct best_bid_ask event (requires custom_feature_enabled)."""
        asset_id = data.get('asset_id', '')
        if not asset_id or asset_id not in self._books:
            return
        book = self._books[asset_id]
        book.best_bid = float(data.get('best_bid', book.best_bid))
        book.best_ask = float(data.get('best_ask', book.best_ask))
        book.spread = float(data.get('spread', 0))
        book.last_update = time.time()
        book.update_count += 1

        for cb in self._on_price_change:
            try:
                cb(asset_id, book.best_bid, book.best_ask, book.last_update)
            except Exception as e:
                log.debug(f'Price callback error: {e}')

    def _handle_last_trade_price(self, data: Dict):
        """A trade executed — update last_trade_price."""
        asset_id = data.get('asset_id', '')
        if not asset_id or asset_id not in self._books:
            return
        price = float(data.get('price', 0))
        if price > 0:
            self._books[asset_id].last_trade_price = price
            self._books[asset_id].last_update = time.time()

    async def _handle_market_resolved(self, data: Dict):
        """A market has resolved — fire callback for L4 to act on."""
        market_cid = data.get('market', '')        # condition_id
        asset_id = data.get('asset_id', '')
        winner = data.get('winner', '')
        log.info(f'WS: market_resolved market={market_cid[:20]} asset={asset_id[:20]} winner={winner}')
        for cb in self._on_market_resolved:
            try:
                cb(market_cid, asset_id)
            except Exception as e:
                log.warning(f'Resolved callback error: {e}')

    async def _handle_user_message(self, data: Dict):
        """Route user channel messages."""
        event_type = data.get('event_type', data.get('type', ''))
        status = data.get('status', '')

        if event_type == 'trade':
            log.debug(f'WS user trade: id={data.get("id","?")} status={status} '
                       f'side={data.get("side","?")} size={data.get("size","?")} '
                       f'price={data.get("price","?")}')
            for cb in self._on_trade_confirm:
                try:
                    cb(data)
                except Exception as e:
                    log.warning(f'Trade callback error: {e}')

        elif event_type == 'order':
            order_type = data.get('type', '')  # PLACEMENT, CANCELLATION, etc.
            log.debug(f'WS user order: id={data.get("id","?")[:20]} type={order_type} '
                       f'side={data.get("side","?")} price={data.get("price","?")} '
                       f'matched={data.get("size_matched","0")}')

        else:
            log.debug(f'WS user: unknown event_type={event_type}')


# --- Convenience: build asset_id list from L2 constraints + market lookup ---

def get_asset_ids_for_constraints(constraints: List[Dict],
                                   market_lookup: Dict) -> List[str]:
    """
    Extract all CLOB token_ids (both YES and NO) for all markets in given constraints.
    Used to build the initial subscription list from L2 output.
    """
    asset_ids = set()
    for constraint in constraints:
        market_ids = constraint.get('market_ids', [])
        for mid in market_ids:
            md = market_lookup.get(str(mid))
            if not md:
                continue
            clob_raw = md.metadata.get('clobTokenIds', '[]') if hasattr(md, 'metadata') else '[]'
            try:
                clob_ids = json.loads(clob_raw) if isinstance(clob_raw, str) else clob_raw
            except (json.JSONDecodeError, TypeError):
                continue
            for tid in (clob_ids or []):
                if tid:
                    asset_ids.add(tid)
    return list(asset_ids)


def get_condition_ids_for_positions(open_positions: Dict) -> List[str]:
    """Extract condition_ids from open positions for user channel subscription."""
    cids = set()
    for pos in open_positions.values():
        if hasattr(pos, 'metadata') and pos.metadata:
            cid = pos.metadata.get('condition_id', '')
            if cid:
                cids.add(cid)
    return list(cids)


if __name__ == '__main__':
    """Quick test: connect to market channel, subscribe to a few assets, print events."""
    import sys

    async def main():
        logging.basicConfig(level=logging.DEBUG,
            format='%(asctime)s [%(name)s] %(levelname)s %(message)s')

        mgr = WebSocketManager()

        # Register simple print callbacks
        mgr.on_price_change(lambda aid, bb, ba, ts:
            log.info(f'PRICE: {aid[:20]}... bid={bb} ask={ba}'))
        mgr.on_book_update(lambda aid, book:
            log.info(f'BOOK: {aid[:20]}... bids={len(book.bids)} asks={len(book.asks)} '
                     f'depth=${book.total_ask_depth_usd():.2f}'))
        mgr.on_market_resolved(lambda mkt, asset:
            log.info(f'RESOLVED: market={mkt[:20]} asset={asset[:20]}'))

        # Test with a known active asset_id (replace with real one)
        # You can find asset_ids from CLOB API: GET /markets?id=<condition_id>
        test_assets = sys.argv[1:] if len(sys.argv) > 1 else []
        if test_assets:
            await mgr.subscribe_assets(test_assets)

        await mgr.start()
        try:
            # Run for 60 seconds then stop
            await asyncio.sleep(60)
        finally:
            await mgr.stop()
            stats = mgr.get_stats()
            log.info(f'Stats: {json.dumps(stats, indent=2)}')

    asyncio.run(main())
