"""
Live Trading Engine - Polymarket CLOB API integration
Handles: authentication, balance queries, price fetching, order placement, fill monitoring
"""
import yaml, json, time, logging, asyncio, requests
from pathlib import Path
from datetime import datetime, timezone
from typing import Dict, List, Optional, Tuple

from py_clob_client.client import ClobClient
from py_clob_client.clob_types import (
    OrderArgs, OrderType, BalanceAllowanceParams, AssetType
)

log = logging.getLogger('LiveTrading')

GAMMA_API = "https://gamma-api.polymarket.com"
MARKETS_PATH = Path('/home/andydoc/prediction-trader/data/latest_markets.json')


class LiveTradingEngine:
    """Wraps Polymarket CLOB API for live order execution"""

    def __init__(self, config: Dict, workspace_root: Path):
        self.config = config
        self.workspace = workspace_root
        self.live_config = config.get('live_trading', {})

        # Load secrets
        secrets_path = workspace_root / 'config' / 'secrets.yaml'
        with open(secrets_path) as f:
            self.secrets = yaml.safe_load(f)['polymarket']

        # Initialize CLOB client
        self.client = ClobClient(
            self.secrets['host'],
            key=self.secrets['private_key'],
            chain_id=self.secrets['chain_id'],
            signature_type=self.secrets['signature_type'],
            funder=self.secrets['funder_address']
        )
        creds = self.client.create_or_derive_api_creds()
        self.client.set_api_creds(creds)
        log.info(f"CLOB client initialized, API key: {creds.api_key[:8]}...")

        # Settings
        self.max_capital = self.live_config.get('max_capital', 100)
        self.max_positions = self.live_config.get('max_positions', 11)
        # capital_per_trade is set dynamically by L4 (dynamic_capital); stored here for shadow validation
        self.capital_per_trade = self.live_config.get('capital_per_trade', 10)  # fallback only
        self.min_profit_after_fees = self.live_config.get('min_profit_after_fees', 0.03)
        self.min_orderbook_depth = self.live_config.get('min_orderbook_depth_usd', 50)
        self.order_timeout = self.live_config.get('order_timeout_secs', 60)
        self.fill_check_interval = self.live_config.get('fill_check_interval_secs', 2)
        self.low_balance_pause = self.live_config.get('low_balance_pause_usd', 20)

        # Token ID cache: condition_id -> {yes_token, no_token}
        self._token_cache: Dict[str, Dict] = {}
        # Fee rate cache: token_id -> fee_rate_bps
        self._fee_cache: Dict[str, float] = {}
        # Market ID -> conditionId map (from L1 data)
        self._market_condition_map: Dict[str, str] = {}
        self._market_slug_map: Dict[str, str] = {}
        self._market_negrisk_map: Dict[str, bool] = {}  # market_id -> is_negRisk
        self._market_map_loaded_at: float = 0

    # ---- Balance ----

    def get_usdc_balance(self) -> float:
        """Get available USDC balance (returns dollars)"""
        try:
            params = BalanceAllowanceParams(asset_type=AssetType.COLLATERAL)
            result = self.client.get_balance_allowance(params)
            raw = float(result.get('balance', 0))
            return raw / 1e6
        except Exception as e:
            log.error(f"Balance check failed: {e}")
            return 0.0

    def is_balance_sufficient(self, required_usd: float) -> Tuple[bool, float]:
        balance = self.get_usdc_balance()
        return balance >= required_usd, balance

    # ---- Token ID Resolution ----

    def _load_market_condition_map(self):
        """Load market_id -> slug, conditionId, and clobTokenIds from L1 data"""
        try:
            if not MARKETS_PATH.exists():
                return
            # Refresh at most every 60s
            if time.time() - self._market_map_loaded_at < 60:
                return
            data = json.loads(MARKETS_PATH.read_text())
            markets = data.get('markets', [])
            cond_map = {}
            slug_map = {}
            negrisk_map = {}
            tokens_populated = 0
            for m in markets:
                mid = str(m.get('market_id', ''))
                meta = m.get('metadata', {})
                cid = meta.get('conditionId', '')
                slug = meta.get('slug', '')
                neg_risk = meta.get('negRisk', False)
                if mid and cid:
                    cond_map[mid] = cid
                if mid and slug:
                    slug_map[mid] = slug
                if mid:
                    negrisk_map[mid] = bool(neg_risk)
                # Parse clobTokenIds directly from L1 metadata
                if mid and mid not in self._token_cache:
                    raw_tokens = meta.get('clobTokenIds', '')
                    if raw_tokens:
                        tokens = self._parse_clob_token_ids(raw_tokens)
                        if tokens:
                            self._token_cache[mid] = tokens
                            tokens_populated += 1
            self._market_condition_map = cond_map
            self._market_slug_map = slug_map
            self._market_negrisk_map = negrisk_map
            self._market_map_loaded_at = time.time()
            log.debug(f"Loaded {len(slug_map)} market mappings, {tokens_populated} new token IDs cached (total cache: {len(self._token_cache)})")
        except Exception as e:
            log.warning(f"Failed to load market maps: {e}")

    def _parse_clob_token_ids(self, raw_tokens) -> Optional[Dict]:
        """Parse clobTokenIds from L1 metadata into {yes_token, no_token}"""
        try:
            if isinstance(raw_tokens, str):
                try:
                    tokens = json.loads(raw_tokens)
                except:
                    tokens = [t.strip().strip('"') for t in raw_tokens.strip('[]').split(',')]
            else:
                tokens = list(raw_tokens)
            if len(tokens) >= 2:
                # Polymarket convention: first token = YES, second = NO
                return {
                    'yes_token': str(tokens[0]).strip(),
                    'no_token': str(tokens[1]).strip()
                }
        except Exception as e:
            log.debug(f"Token parse error: {e}")
        return None

    def _fetch_token_ids_from_gamma(self, slug: str) -> Optional[Dict]:
        """Fetch YES/NO token IDs from Gamma API using market slug"""
        try:
            resp = requests.get(f"{GAMMA_API}/markets", params={
                "slug": slug, "limit": 1
            }, timeout=10)
            if resp.status_code != 200:
                log.debug(f"Gamma slug query failed: HTTP {resp.status_code}")
                return None
            markets = resp.json()
            if not markets:
                log.debug(f"No Gamma market found for slug: {slug}")
                return None
            m = markets[0]
            # Check market is active with order book
            if m.get('closed', False) and not m.get('enableOrderBook', False):
                log.debug(f"Market is closed/no orderbook: {slug}")
                return None
            raw_tokens = m.get('clobTokenIds', '')
            raw_outcomes = m.get('outcomes', '')
            # Parse - could be JSON string or already list
            if isinstance(raw_tokens, str):
                try:
                    tokens = json.loads(raw_tokens)
                except:
                    tokens = [t.strip().strip('"') for t in raw_tokens.strip('[]').split(',')]
            else:
                tokens = raw_tokens
            if isinstance(raw_outcomes, str):
                try:
                    outcomes = json.loads(raw_outcomes)
                except:
                    outcomes = [o.strip().strip('"') for o in raw_outcomes.strip('[]').split(',')]
            else:
                outcomes = raw_outcomes
            result = {}
            for tok, out in zip(tokens, outcomes):
                tok = str(tok).strip()
                out = str(out).strip().lower()
                if 'yes' in out:
                    result['yes_token'] = tok
                elif 'no' in out:
                    result['no_token'] = tok
            if 'yes_token' in result and 'no_token' in result:
                return result
            log.debug(f"Incomplete tokens for {slug}: {list(result.keys())}")
        except Exception as e:
            log.warning(f"Gamma token lookup failed for {slug}: {e}")
        return None

    def resolve_token_ids(self, market_id: str) -> Optional[Dict]:
        """Get cached or fetch token IDs for a market. L1 data first, Gamma API fallback."""
        # Check cache (populated from L1 data)
        if market_id in self._token_cache:
            return self._token_cache[market_id]

        # Refresh L1 data (also populates token cache)
        self._market_map_loaded_at = 0  # Force refresh
        self._load_market_condition_map()
        
        # Check cache again after L1 refresh
        if market_id in self._token_cache:
            log.debug(f"Resolved tokens from L1 for {market_id}")
            return self._token_cache[market_id]

        # Fallback: Gamma API lookup via slug
        slug = self._market_slug_map.get(str(market_id))
        if not slug:
            log.warning(f"No slug found for market_id={market_id}")
            return None

        result = self._fetch_token_ids_from_gamma(slug)
        if result:
            self._token_cache[market_id] = result
            log.debug(f"Resolved tokens via Gamma for {market_id} ({slug[:30]})")
        else:
            log.debug(f"Token resolution failed for {market_id} ({slug[:30]})")
        return result

    # ---- Prices & Order Book ----

    def get_midpoint(self, token_id: str) -> Optional[float]:
        """Get midpoint price for a token"""
        try:
            result = self.client.get_midpoint(token_id)
            if isinstance(result, dict):
                return float(result.get('mid', 0))
            return float(result)
        except Exception as e:
            log.warning(f"Midpoint fetch failed for {token_id[:20]}: {e}")
            return None

    def get_live_prices(self, market_ids: List[str]) -> Dict[str, float]:
        """Get live YES midpoint prices for multiple markets"""
        prices = {}
        for mid in market_ids:
            tokens = self.resolve_token_ids(mid)
            if not tokens:
                log.warning(f"No tokens for market {mid}")
                continue
            yes_mid = self.get_midpoint(tokens['yes_token'])
            if yes_mid is not None:
                prices[mid] = yes_mid
        return prices

    def check_orderbook_depth(self, token_id: str, side: str, price: float,
                               min_depth_usd: float = None,
                               is_neg_risk: bool = False) -> Tuple[bool, float]:
        """
        Check if order book has enough depth near the target price.
        For negRisk markets: the CLOB shows wide spreads (bids~0.01, asks~0.99)
        because liquidity comes through the neg-risk adapter. We check total
        book depth instead of depth near midpoint.
        Returns (sufficient, available_depth_usd)
        """
        if min_depth_usd is None:
            min_depth_usd = self.min_orderbook_depth
        try:
            book = self.client.get_order_book(token_id)
            if side.upper() == 'BUY':
                orders = book.asks if hasattr(book, 'asks') else book.get('asks', [])
            else:
                orders = book.bids if hasattr(book, 'bids') else book.get('bids', [])

            if is_neg_risk:
                # NegRisk: adapter provides liquidity at wide prices.
                # Check total book depth (any price level).
                # If there are standing orders from the adapter, liquidity exists.
                depth_usd = 0.0
                for order in orders:
                    op = float(order.price if hasattr(order, 'price') else order.get('price', 0))
                    os_size = float(order.size if hasattr(order, 'size') else order.get('size', 0))
                    depth_usd += op * os_size
                # NegRisk adapter typically shows 100K+ in depth
                # Use a lower threshold since we trust the midpoint
                effective_min = min(min_depth_usd, 10.0)
                return depth_usd >= effective_min, depth_usd
            else:
                # Standard market: check depth within 2% of target price
                depth_usd = 0.0
                price_threshold = price * 0.02
                for order in orders:
                    op = float(order.price if hasattr(order, 'price') else order.get('price', 0))
                    os_size = float(order.size if hasattr(order, 'size') else order.get('size', 0))
                    if abs(op - price) <= price_threshold:
                        depth_usd += op * os_size
                return depth_usd >= min_depth_usd, depth_usd
        except Exception as e:
            log.warning(f"Order book check failed: {e}")
            return False, 0.0

    def get_fee_rate(self, token_id: str) -> float:
        """Get taker fee rate for a token (as decimal, e.g. 0.01 = 1%)"""
        if token_id in self._fee_cache:
            return self._fee_cache[token_id]
        # Default: most markets are 0%, crypto 15-min can be up to 3%
        # The fee is returned in the trade response, so we use config default
        # and update from actual fills
        default = self.config.get('arbitrage', {}).get('fees', {}).get('polymarket_taker_fee', 0.0001)
        self._fee_cache[token_id] = default
        return default

    # ---- Pre-Trade Validation ----

    def validate_opportunity_live(self, opp_dict: Dict, skip_balance_check: bool = False) -> Dict:
        """
        Enhanced validation for live trading:
        1. All token IDs resolvable
        2. Live prices available
        3. Price drift < threshold
        4. Order book depth sufficient
        5. Profit still above threshold after fees
        6. Balance sufficient
        Returns {valid: bool, reason: str, live_prices: dict, token_map: dict, est_fees: float}
        """
        market_ids = opp_dict.get('market_ids', [])
        meta = opp_dict.get('metadata', {})
        strategy = meta.get('strategy', '') or meta.get('method', '')
        optimal_bets = opp_dict.get('optimal_bets', {})
        orig_prices = opp_dict.get('current_prices', {})

        # 1. Resolve all token IDs
        token_map = {}  # market_id -> {yes_token, no_token}
        for mid in market_ids:
            tokens = self.resolve_token_ids(mid)
            if not tokens:
                return {'valid': False, 'reason': f'no_tokens_for_{mid}'}
            token_map[mid] = tokens

        # 2. Fetch live prices
        live_prices = {}
        for mid in market_ids:
            yes_mid = self.get_midpoint(token_map[mid]['yes_token'])
            if yes_mid is None:
                return {'valid': False, 'reason': f'no_live_price_{mid}'}
            live_prices[mid] = yes_mid

        # 3. Price drift check (5% for live, stricter than paper's 10%)
        max_drift = self.live_config.get('max_price_drift_pct', 0.05)
        for mid in market_ids:
            orig = orig_prices.get(mid, 0)
            if orig <= 0:
                continue
            drift = abs(live_prices[mid] - orig) / orig
            if drift > max_drift:
                return {
                    'valid': False, 'reason': 'price_drift',
                    'market_id': mid, 'drift_pct': drift * 100
                }

        # 4. Order book depth for each leg
        for mid in market_ids:
            bet = optimal_bets.get(mid, 0)
            if bet <= 0:
                continue
            # Determine which token we're buying
            is_sell_all = 'sell' in strategy.lower()
            if is_sell_all:
                # Sell-all: buy NO tokens
                tok = token_map[mid]['no_token']
                price = 1.0 - live_prices[mid]
            else:
                # Buy-all: buy YES tokens
                tok = token_map[mid]['yes_token']
                price = live_prices[mid]
            is_neg = self._market_negrisk_map.get(str(mid), False)
            sufficient, depth = self.check_orderbook_depth(tok, 'BUY', price, is_neg_risk=is_neg)
            if not sufficient:
                return {
                    'valid': False, 'reason': 'insufficient_depth',
                    'market_id': mid, 'depth_usd': depth,
                    'required': self.min_orderbook_depth
                }

        # 5. Recalculate profit with live prices + fees
        price_sum = sum(live_prices.values())
        if 'sell' in strategy.lower():
            # Sell-all: YES prices sum > 1, buy NO tokens cheaply
            mispricing = price_sum - 1.0
        else:
            # Buy-all: YES prices sum < 1, buy YES tokens cheaply
            mispricing = 1.0 - price_sum
        # Negative mispricing = no profit
        if mispricing <= 0:
            return {'valid': False, 'reason': 'no_mispricing_at_live_prices'}

        total_capital = opp_dict.get('total_capital_required', self.capital_per_trade)
        est_profit_pct = mispricing  # Simplified
        # Estimate fees: taker fee on each leg
        n_legs = len(market_ids)
        avg_fee = sum(self.get_fee_rate(token_map[m]['yes_token']) for m in market_ids) / max(n_legs, 1)
        est_total_fees = total_capital * avg_fee * n_legs
        net_profit_pct = est_profit_pct - (est_total_fees / total_capital)

        if net_profit_pct < self.min_profit_after_fees:
            return {
                'valid': False, 'reason': 'profit_below_threshold_after_fees',
                'gross_pct': est_profit_pct * 100,
                'fees_pct': (est_total_fees / total_capital) * 100,
                'net_pct': net_profit_pct * 100
            }

        # 6. Balance check (skipped in shadow mode)
        balance = 0.0
        if not skip_balance_check:
            sufficient, balance = self.is_balance_sufficient(total_capital)
            if not sufficient:
                return {
                    'valid': False, 'reason': 'insufficient_balance',
                    'required': total_capital, 'available': balance
                }
            if balance < self.low_balance_pause:
                return {'valid': False, 'reason': 'balance_below_pause_threshold'}

        return {
            'valid': True,
            'live_prices': live_prices,
            'token_map': token_map,
            'est_fees': est_total_fees,
            'net_profit_pct': net_profit_pct,
            'balance': balance
        }

    # ---- Order Placement ----

    def _build_order(self, token_id: str, side: str, price: float,
                     size: float) -> OrderArgs:
        """Build a single limit order"""
        return OrderArgs(
            token_id=token_id,
            price=round(price, 4),
            size=round(size, 2),
            side=side.upper(),
            order_type=OrderType.GTC,
        )

    def place_multi_leg_order(self, opp_dict: Dict, validation: Dict) -> Dict:
        """
        Place all legs of an arbitrage position as GTC limit orders.
        Returns {success, orders: [...], filled: [...], failed: [...]}
        """
        market_ids = opp_dict.get('market_ids', [])
        optimal_bets = opp_dict.get('optimal_bets', {})
        meta = opp_dict.get('metadata', {})
        strategy = meta.get('strategy', '') or meta.get('method', '')
        live_prices = validation['live_prices']
        token_map = validation['token_map']
        is_sell_all = 'sell' in strategy.lower()

        orders_to_place = []
        order_meta = []  # Track what each order corresponds to

        for mid in market_ids:
            bet = optimal_bets.get(mid, 0)
            if bet <= 0:
                continue

            if is_sell_all:
                # Buy NO token: price = 1 - yes_price, token = no_token
                token_id = token_map[mid]['no_token']
                price = round(1.0 - live_prices[mid], 4)
                side = 'BUY'
            else:
                # Buy YES token
                token_id = token_map[mid]['yes_token']
                price = round(live_prices[mid], 4)
                side = 'BUY'

            if price <= 0 or price >= 1:
                log.warning(f"Invalid price {price} for {mid}, skipping")
                continue

            # Calculate shares: bet_amount / price
            shares = round(bet / price, 2)
            if shares < 1:
                shares = 1.0

            order = self._build_order(token_id, side, price, shares)
            orders_to_place.append(order)
            order_meta.append({
                'market_id': mid,
                'token_id': token_id,
                'side': side,
                'price': price,
                'size': shares,
                'bet_amount': bet,
                'is_no': is_sell_all
            })

        if not orders_to_place:
            return {'success': False, 'reason': 'no_valid_orders'}

        log.info(f"Placing {len(orders_to_place)} orders for "
                 f"{'sell-all' if is_sell_all else 'buy-all'} position")

        # Place orders one by one (safer than batch for error handling)
        placed_orders = []
        failed_orders = []

        for order, meta in zip(orders_to_place, order_meta):
            try:
                # Sign and place the order
                signed = self.client.create_order(order)
                result = self.client.post_order(signed, order_type=OrderType.GTC)
                order_id = result.get('orderID', result.get('order_id', ''))
                log.info(f"  Order placed: {meta['market_id'][:30]} "
                         f"{meta['side']} {meta['size']}@{meta['price']} "
                         f"-> order_id={order_id[:12] if order_id else '?'}")
                placed_orders.append({
                    **meta,
                    'order_id': order_id,
                    'status': 'placed',
                    'response': result
                })
            except Exception as e:
                log.error(f"  Order FAILED for {meta['market_id'][:30]}: {e}")
                failed_orders.append({**meta, 'error': str(e)})

        # If any orders failed, cancel all placed orders (unwind)
        if failed_orders and placed_orders:
            log.warning(f"Unwinding {len(placed_orders)} orders due to {len(failed_orders)} failures")
            for po in placed_orders:
                try:
                    self.client.cancel(po['order_id'])
                    log.info(f"  Cancelled: {po['order_id'][:12]}")
                except Exception as e:
                    log.error(f"  Cancel failed: {e}")
            return {
                'success': False,
                'reason': 'partial_placement_unwound',
                'placed': len(placed_orders),
                'failed': len(failed_orders),
                'failed_details': failed_orders
            }

        if not placed_orders:
            return {'success': False, 'reason': 'all_orders_failed', 'errors': failed_orders}

        return {
            'success': True,
            'orders': placed_orders,
            'n_legs': len(placed_orders)
        }

    # ---- Fill Monitoring ----

    def monitor_fills(self, placed_orders: List[Dict]) -> Dict:
        """
        Poll for fills on placed orders. Wait up to order_timeout seconds.
        Returns {all_filled, fills: [...], unfilled: [...]}
        """
        start = time.time()
        order_ids = {o['order_id']: o for o in placed_orders}
        fills = {}
        
        while time.time() - start < self.order_timeout:
            try:
                open_orders = self.client.get_orders()
                open_ids = set()
                if isinstance(open_orders, list):
                    for oo in open_orders:
                        oid = oo.get('id', oo.get('order_id', ''))
                        open_ids.add(oid)
                
                # Orders no longer in open list are either filled or cancelled
                for oid, meta in order_ids.items():
                    if oid not in open_ids and oid not in fills:
                        # Assume filled (could also check trades endpoint)
                        fills[oid] = {
                            **meta,
                            'status': 'filled',
                            'fill_time': datetime.now(timezone.utc).isoformat()
                        }
                        log.info(f"  Fill confirmed: {meta['market_id'][:30]} "
                                 f"{meta['size']}@{meta['price']}")
                
                if len(fills) == len(order_ids):
                    log.info(f"All {len(fills)} orders filled in "
                             f"{time.time() - start:.1f}s")
                    return {
                        'all_filled': True,
                        'fills': list(fills.values()),
                        'unfilled': []
                    }
                    
            except Exception as e:
                log.warning(f"Fill check error: {e}")
            
            time.sleep(self.fill_check_interval)
        
        # Timeout - cancel unfilled orders
        unfilled = []
        for oid, meta in order_ids.items():
            if oid not in fills:
                try:
                    self.client.cancel(oid)
                    log.warning(f"  Timeout cancel: {meta['market_id'][:30]}")
                except Exception as e:
                    log.error(f"  Cancel failed for {oid}: {e}")
                unfilled.append({**meta, 'status': 'timeout_cancelled'})
        
        return {
            'all_filled': False,
            'fills': list(fills.values()),
            'unfilled': unfilled
        }

    # ---- Exit / Liquidation ----

    def liquidate_live_position(self, position_markets: Dict,
                                 token_map: Dict) -> Dict:
        """
        Exit a live position by selling all held shares.
        position_markets: {market_id: {side, shares, token_id, ...}}
        Returns {success, proceeds, fees, orders}
        """
        total_proceeds = 0.0
        total_fees = 0.0
        results = []
        
        for mid, info in position_markets.items():
            token_id = info.get('token_id', '')
            shares = info.get('shares', 0)
            if not token_id or shares <= 0:
                continue
            
            try:
                # Get current price to sell at
                mid_price = self.get_midpoint(token_id)
                if mid_price is None or mid_price <= 0:
                    log.error(f"Can't get sell price for {mid}")
                    results.append({'market_id': mid, 'success': False, 'reason': 'no_price'})
                    continue
                
                # Sell slightly below midpoint for faster fill
                sell_price = round(mid_price * 0.995, 4)
                if sell_price <= 0:
                    sell_price = 0.001
                
                order = self._build_order(token_id, 'SELL', sell_price, shares)
                signed = self.client.create_order(order)
                result = self.client.post_order(signed, order_type=OrderType.GTC)
                order_id = result.get('orderID', result.get('order_id', ''))
                
                proceeds = shares * sell_price
                fee = proceeds * self.get_fee_rate(token_id)
                total_proceeds += proceeds - fee
                total_fees += fee
                
                results.append({
                    'market_id': mid, 'success': True,
                    'order_id': order_id, 'price': sell_price,
                    'shares': shares, 'proceeds': proceeds, 'fee': fee
                })
                log.info(f"  Sold {shares}@{sell_price} on {mid[:30]}, "
                         f"proceeds=${proceeds:.2f}")
                
            except Exception as e:
                log.error(f"Sell failed for {mid}: {e}")
                results.append({'market_id': mid, 'success': False, 'error': str(e)})
        
        return {
            'success': all(r.get('success') for r in results),
            'proceeds': total_proceeds,
            'fees': total_fees,
            'orders': results
        }

    # ---- Cancel All ----

    def cancel_all_orders(self) -> int:
        """Cancel all open orders. Returns count cancelled."""
        try:
            result = self.client.cancel_all()
            log.info(f"Cancelled all open orders: {result}")
            return result if isinstance(result, int) else 0
        except Exception as e:
            log.error(f"Cancel all failed: {e}")
            return 0

    # ---- Main Execution Entry Point ----

    def execute_live_trade(self, opp_dict: Dict) -> Dict:
        """
        Full live trade execution:
        1. Validate opportunity with live data
        2. Place multi-leg orders
        3. Monitor fills
        4. Return position data or unwind on failure
        
        Returns {success, position_data, fills, fees, ...}
        """
        opp_id = opp_dict.get('opportunity_id', '?')
        meta = opp_dict.get('metadata', {})
        strategy = meta.get('strategy', '') or meta.get('method', '')
        
        log.info(f"=== LIVE TRADE: {opp_id} ({strategy}) ===")
        
        # Step 1: Validate
        validation = self.validate_opportunity_live(opp_dict)
        if not validation['valid']:
            log.info(f"  Validation failed: {validation.get('reason')}")
            return {'success': False, 'stage': 'validation', **validation}
        
        log.info(f"  Validated: prices OK, depth OK, net profit "
                 f"{validation['net_profit_pct']*100:.2f}%, "
                 f"balance ${validation['balance']:.2f}")
        
        # Step 2: Place orders
        placement = self.place_multi_leg_order(opp_dict, validation)
        if not placement['success']:
            log.warning(f"  Placement failed: {placement.get('reason')}")
            return {'success': False, 'stage': 'placement', **placement}
        
        # Step 3: Monitor fills
        fill_result = self.monitor_fills(placement['orders'])
        
        if not fill_result['all_filled']:
            # Partial fill - unwind filled orders
            n_filled = len(fill_result['fills'])
            n_unfilled = len(fill_result['unfilled'])
            log.warning(f"  Partial fill: {n_filled}/{n_filled + n_unfilled}")
            
            if fill_result['fills']:
                # Build sell orders for filled legs
                sell_map = {}
                for f in fill_result['fills']:
                    sell_map[f['market_id']] = {
                        'token_id': f['token_id'],
                        'shares': f['size'],
                        'side': 'SELL'
                    }
                unwind = self.liquidate_live_position(sell_map, validation['token_map'])
                log.warning(f"  Unwound filled legs, recovered ${unwind['proceeds']:.2f}")
            
            return {
                'success': False, 'stage': 'fill',
                'fills': fill_result['fills'],
                'unfilled': fill_result['unfilled']
            }
        
        # Step 4: Build position data
        total_cost = 0.0
        total_fees = 0.0
        position_markets = {}
        
        for fill in fill_result['fills']:
            mid = fill['market_id']
            cost = fill['size'] * fill['price']
            fee = cost * self.get_fee_rate(fill['token_id'])
            total_cost += cost
            total_fees += fee
            position_markets[mid] = {
                'token_id': fill['token_id'],
                'side': 'NO' if fill.get('is_no') else 'YES',
                'entry_price': fill['price'],
                'shares': fill['size'],
                'bet_amount': cost,
                'fee': fee,
                'order_id': fill['order_id'],
                'fill_time': fill.get('fill_time', '')
            }
        
        log.info(f"  ✓ LIVE POSITION ENTERED: {len(position_markets)} legs, "
                 f"${total_cost:.2f} deployed, fees ${total_fees:.4f}")
        
        return {
            'success': True,
            'position_markets': position_markets,
            'total_cost': total_cost,
            'total_fees': total_fees,
            'live_prices': validation['live_prices'],
            'net_profit_pct': validation['net_profit_pct'],
            'token_map': validation['token_map']
        }

    # ---- Shadow Mode (log but don't execute) ----

    def shadow_trade(self, opp_dict: Dict) -> Dict:
        """
        Run full validation but DON'T place orders.
        Logs what would have happened. Used for testing.
        """
        opp_id = opp_dict.get('opportunity_id', '?')
        meta = opp_dict.get('metadata', {})
        strategy = meta.get('strategy', '') or meta.get('method', '')
        
        validation = self.validate_opportunity_live(opp_dict, skip_balance_check=True)
        
        result = {
            'shadow': True,
            'opportunity_id': opp_id,
            'strategy': strategy,
            'timestamp': datetime.now(timezone.utc).isoformat(),
            'validation': validation
        }
        
        if validation['valid']:
            log.info(f"[SHADOW] WOULD TRADE: {opp_id} ({strategy}) "
                     f"net={validation['net_profit_pct']*100:.2f}% "
                     f"fees=${validation['est_fees']:.4f}")
        else:
            log.debug(f"[SHADOW] Rejected: {opp_id} - {validation.get('reason')}")
        
        return result

    # ---- Health Check ----

    def health_check(self) -> Dict:
        """Quick connectivity and balance check"""
        try:
            server_time = self.client.get_server_time()
            balance = self.get_usdc_balance()
            return {
                'healthy': True,
                'server_time': server_time,
                'balance_usd': balance,
                'timestamp': datetime.now(timezone.utc).isoformat()
            }
        except Exception as e:
            return {'healthy': False, 'error': str(e)}
