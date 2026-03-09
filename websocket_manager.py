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
SUB_BATCH_SIZE = 500         # max assets per subscription message
ASSETS_PER_CONNECTION = 4000 # max assets per WS connection (prevents data flood)


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
# on_new_market(new_market_data_dict)
NewMarketCallback = Callable[[Dict], None]


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
        self._resolved_assets: Set[str] = set()          # asset_ids resolved via WS (pruned from bridge)
        # Market channel pool: multiple connections, each ≤ ASSETS_PER_CONNECTION
        self._market_shards: Dict[int, dict] = {}        # shard_id -> {ws, task, heartbeat_task, assets: set}
        self._next_shard_id = 0
        self._user_ws = None
        self._user_task: Optional[asyncio.Task] = None
        self._heartbeat_task_user: Optional[asyncio.Task] = None
        self._running = False

        # --- Callbacks ---
        self._on_price_change: List[PriceCallback] = []
        self._on_book_update: List[BookCallback] = []
        self._on_trade_confirm: List[TradeCallback] = []
        self._on_market_resolved: List[ResolvedCallback] = []
        self._on_new_market: List[NewMarketCallback] = []

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

    def on_new_market(self, cb: NewMarketCallback):
        self._on_new_market.append(cb)

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
        s = dict(self._stats)
        s['resolved_assets'] = len(self._resolved_assets)
        s['active_books'] = len(self._books)
        return s

    def export_price_cache(self) -> Dict[str, Dict]:
        """Export all local book prices as {asset_id: {best_bid, best_ask, mid, last_trade, ts, updates}}.
        Used by L4 to build the ws_prices.json bridge file for L3."""
        cache = {}
        now = time.time()
        for aid, book in self._books.items():
            if book.update_count == 0:
                continue  # No data received yet
            if aid in self._resolved_assets:
                continue  # Resolved — don't export stale prices
            mid = round((book.best_bid + book.best_ask) / 2, 6) if (book.best_bid and book.best_ask) else 0.0
            cache[aid] = {
                'best_bid': book.best_bid,
                'best_ask': book.best_ask,
                'mid': mid,
                'last_trade': book.last_trade_price,
                'ts': book.last_update,
                'age_secs': round(now - book.last_update, 1) if book.last_update else -1,
                'updates': book.update_count,
            }
        return cache

    # --- Public: subscription management ---

    async def _send_batched_subscribe(self, ws, asset_ids: list, is_initial: bool = False):
        """Send subscription in batches of SUB_BATCH_SIZE to avoid overwhelming the server.
        Uses initial_dump=false to skip massive book snapshots on subscribe."""
        batches = [asset_ids[i:i + SUB_BATCH_SIZE] for i in range(0, len(asset_ids), SUB_BATCH_SIZE)]
        for idx, batch in enumerate(batches):
            if is_initial and idx == 0:
                # First batch on connect uses 'type' field
                msg = json.dumps({
                    'assets_ids': batch,
                    'type': 'market',
                    'custom_feature_enabled': True,
                    'initial_dump': False,
                })
            else:
                # Subsequent batches use 'operation' field (dynamic subscribe)
                msg = json.dumps({
                    'assets_ids': batch,
                    'operation': 'subscribe',
                    'custom_feature_enabled': True,
                    'initial_dump': False,
                })
            await ws.send(msg)
            if len(batches) > 1:
                log.debug(f'WS: subscription batch {idx+1}/{len(batches)} ({len(batch)} assets)')
                await asyncio.sleep(0.5)  # Brief pause between batches
        log.info(f'WS market: subscribed {len(asset_ids)} assets in {len(batches)} batch(es)')

    async def subscribe_assets(self, asset_ids: List[str]):
        """Add asset_ids to market channel subscription. Routes to existing shards or creates new ones."""
        new_ids = [aid for aid in asset_ids if aid not in self._subscribed_assets]
        if not new_ids:
            return
        self._subscribed_assets.update(new_ids)
        # Initialize empty books
        for aid in new_ids:
            if aid not in self._books:
                self._books[aid] = LocalOrderBook(asset_id=aid)

        # Distribute to shards
        remaining = list(new_ids)
        # Fill existing shards first
        for sid, shard in self._market_shards.items():
            if not remaining:
                break
            capacity = ASSETS_PER_CONNECTION - len(shard['assets'])
            if capacity > 0:
                batch = remaining[:capacity]
                remaining = remaining[capacity:]
                shard['assets'].update(batch)
                ws = shard.get('ws')
                if ws:
                    try:
                        await self._send_batched_subscribe(ws, batch, is_initial=False)
                    except Exception as e:
                        log.warning(f'WS shard {sid}: subscribe send error: {e}')

        # Create new shards for overflow
        while remaining:
            batch = remaining[:ASSETS_PER_CONNECTION]
            remaining = remaining[ASSETS_PER_CONNECTION:]
            sid = self._next_shard_id
            self._next_shard_id += 1
            self._market_shards[sid] = {
                'ws': None, 'task': None, 'heartbeat_task': None,
                'assets': set(batch),
            }
            if self._running:
                self._market_shards[sid]['task'] = asyncio.create_task(
                    self._market_shard_loop(sid))
            log.info(f'WS: created shard {sid} with {len(batch)} assets')

    async def unsubscribe_assets(self, asset_ids: List[str]):
        """Remove asset_ids from market channel subscription."""
        to_remove = [aid for aid in asset_ids if aid in self._subscribed_assets]
        if not to_remove:
            return
        self._subscribed_assets -= set(to_remove)
        for aid in to_remove:
            self._books.pop(aid, None)
        # Remove from shards and send unsubscribe
        for sid, shard in self._market_shards.items():
            shard_remove = [a for a in to_remove if a in shard['assets']]
            if shard_remove:
                shard['assets'] -= set(shard_remove)
                ws = shard.get('ws')
                if ws:
                    try:
                        msg = json.dumps({'assets_ids': shard_remove, 'operation': 'unsubscribe'})
                        await ws.send(msg)
                    except Exception:
                        pass
        log.info(f'WS: unsubscribed {len(to_remove)} assets (total={len(self._subscribed_assets)})')

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
        """Start WebSocket connections as background tasks."""
        if not self.enabled:
            log.info('WS: disabled in config')
            return
        if self._running:
            return
        self._running = True

        # Create initial shards from pre-subscribed assets
        if self._subscribed_assets and not self._market_shards:
            all_assets = list(self._subscribed_assets)
            for i in range(0, len(all_assets), ASSETS_PER_CONNECTION):
                batch = all_assets[i:i + ASSETS_PER_CONNECTION]
                sid = self._next_shard_id
                self._next_shard_id += 1
                self._market_shards[sid] = {
                    'ws': None, 'task': None, 'heartbeat_task': None,
                    'assets': set(batch),
                }

        # Launch shard tasks
        for sid in self._market_shards:
            self._market_shards[sid]['task'] = asyncio.create_task(
                self._market_shard_loop(sid))
        log.info(f'WS: launching {len(self._market_shards)} market shard(s)')

        if self._user_auth:
            self._user_task = asyncio.create_task(self._user_channel_loop())
        else:
            log.info('WS user channel: no API credentials, skipping')
        log.info('WS: started')

    async def stop(self):
        """Gracefully close all connections."""
        self._running = False
        # Cancel all shard tasks
        for sid, shard in self._market_shards.items():
            for key in ('task', 'heartbeat_task'):
                t = shard.get(key)
                if t and not t.done():
                    t.cancel()
                    try:
                        await t
                    except asyncio.CancelledError:
                        pass
            ws = shard.get('ws')
            if ws:
                try:
                    await ws.close()
                except Exception:
                    pass
        self._market_shards.clear()
        # Cancel user channel
        for task in [self._user_task, self._heartbeat_task_user]:
            if task and not task.done():
                task.cancel()
                try:
                    await task
                except asyncio.CancelledError:
                    pass
        if self._user_ws:
            try:
                await self._user_ws.close()
            except Exception:
                pass
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

    async def _market_shard_loop(self, shard_id: int):
        """Persistent market channel connection for one shard with auto-reconnect."""
        delay = self.reconnect_base
        shard = self._market_shards.get(shard_id)
        if not shard:
            return
        while self._running:
            # Wait until shard has assets
            while self._running and not shard['assets']:
                await asyncio.sleep(1.0)
            if not self._running:
                break
            try:
                async with websockets.connect(
                        self.market_url,
                        ping_interval=None,
                        ping_timeout=None,
                        close_timeout=5,
                        max_size=2**22,
                ) as ws:
                    shard['ws'] = ws
                    delay = self.reconnect_base
                    self._stats['reconnects_market'] += 1
                    log.info(f'WS shard {shard_id}: connected ({len(shard["assets"])} assets)')

                    # Start heartbeat
                    shard['heartbeat_task'] = asyncio.create_task(
                        self._heartbeat_loop(ws, f'market-{shard_id}'))

                    # Send initial subscription (batched)
                    if shard['assets']:
                        await self._send_batched_subscribe(
                            ws, list(shard['assets']), is_initial=True)

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
                log.warning(f'WS shard {shard_id}: disconnected ({e}), reconnecting in {delay:.1f}s')
            except asyncio.CancelledError:
                break
            except Exception as e:
                log.error(f'WS shard {shard_id}: unexpected error: {e}', exc_info=True)

            shard['ws'] = None
            hb = shard.get('heartbeat_task')
            if hb:
                hb.cancel()
            if self._running:
                await asyncio.sleep(delay)
                delay = min(delay * 2, self.reconnect_max)

    # --- User Channel ---

    async def _user_channel_loop(self):
        """Persistent user channel connection with auto-reconnect."""
        delay = self.reconnect_base
        while self._running:
            # Wait until we have markets to subscribe to
            while self._running and not self._subscribed_markets:
                await asyncio.sleep(1.0)
            if not self._running:
                break
            try:
                async with websockets.connect(
                        self.user_url,
                        ping_interval=None,
                        ping_timeout=None,
                        close_timeout=5,
                ) as ws:
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
        elif event_type == 'new_market':
            self._handle_new_market(data)
        elif event_type == 'tick_size_change':
            pass  # Known event, no action needed
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
        """A market has resolved — mark resolved, prune from books, fire callback."""
        market_cid = data.get('market', '')        # condition_id
        asset_id = data.get('asset_id', '')
        winner = data.get('winner', '')
        log.info(f'WS: market_resolved market={market_cid[:20]} asset={asset_id[:20]} winner={winner}')

        # Mark as resolved and remove from local book (prevents stale prices in bridge)
        if asset_id:
            self._resolved_assets.add(asset_id)
            self._books.pop(asset_id, None)
        # Fire callbacks (L4 uses this for immediate resolution check)
        for cb in self._on_market_resolved:
            try:
                cb(market_cid, asset_id)
            except Exception as e:
                log.warning(f'Resolved callback error: {e}')

    def _handle_new_market(self, data: Dict):
        """A new market was created — fire callbacks for constraint rebuild."""
        market_id = data.get('id', '')
        question = data.get('question', '?')[:60]
        log.info(f'WS: new_market id={market_id} "{question}"')
        for cb in self._on_new_market:
            try:
                cb(data)
            except Exception as e:
                log.warning(f'New market callback error: {e}')

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
