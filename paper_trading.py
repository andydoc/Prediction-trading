"""
Complete Paper Trading Engine with Market Resolution Monitoring

This implements TRUE paper trading:
1. Read real market data
2. Generate signals (arbitrage opportunities)
3. Validate prices haven't moved before entry
4. Record paper positions at real current prices
5. Monitor markets continuously for resolution
6. Auto-close positions when markets resolve
7. Calculate actual PnL based on real outcomes
8. Compare expected vs actual profit
"""

import asyncio
import time
import logging
from dataclasses import dataclass, field
from datetime import datetime, timezone, timedelta
from typing import Dict, List, Optional, Set
from enum import Enum
from pathlib import Path
import json

try:
    from state_store import StateStore
    HAS_SQLITE = True
except ImportError:
    HAS_SQLITE = False


class PositionStatus(Enum):
    OPEN = "open"
    MONITORING = "monitoring"
    CLOSED = "closed"
    EXPIRED = "expired"


@dataclass
class PaperPosition:
    """
    A complete paper trading position with full lifecycle tracking
    """
    position_id: str
    opportunity_id: str
    
    # Entry details
    markets: Dict[str, Dict]  # market_id -> {name, entry_price, bet_amount, outcome}
    total_capital: float
    expected_profit: float
    expected_profit_pct: float
    fees_paid: float
    
    # Entry conditions
    entry_timestamp: datetime
    entry_prices: Dict[str, float]  # market_id -> price when entered
    
    # Monitoring
    status: PositionStatus = PositionStatus.OPEN
    last_check: datetime = field(default_factory=lambda: datetime.now(timezone.utc))
    price_drift: Dict[str, float] = field(default_factory=dict)  # market_id -> drift since entry
    
    # Resolution
    resolved_at: Optional[datetime] = None
    close_timestamp: Optional[float] = None  # Unix timestamp for fast comparison
    winning_market: Optional[str] = None
    actual_payout: float = 0.0
    actual_profit: float = 0.0
    actual_profit_pct: float = 0.0
    
    # Tracking
    profit_delta: float = 0.0  # actual - expected
    profit_accuracy: float = 0.0  # actual / expected
    
    metadata: Dict = field(default_factory=dict)
    
    def to_dict(self) -> Dict:
        return {
            'position_id': self.position_id,
            'opportunity_id': self.opportunity_id,
            'markets': self.markets,
            'total_capital': self.total_capital,
            'expected_profit': self.expected_profit,
            'expected_profit_pct': self.expected_profit_pct,
            'fees_paid': self.fees_paid,
            'entry_timestamp': self.entry_timestamp.isoformat(),
            'entry_prices': self.entry_prices,
            'status': self.status.value,
            'last_check': self.last_check.isoformat(),
            'price_drift': self.price_drift,
            'resolved_at': self.resolved_at.isoformat() if self.resolved_at else None,
            'close_timestamp': self.close_timestamp,
            'winning_market': self.winning_market,
            'actual_payout': self.actual_payout,
            'actual_profit': self.actual_profit,
            'actual_profit_pct': self.actual_profit_pct,
            'profit_delta': self.profit_delta,
            'profit_accuracy': self.profit_accuracy,
            'metadata': self.metadata
        }


class PaperTradingEngine:
    """
    Complete paper trading engine that:
    - Validates prices before entry
    - Records positions at real current prices
    - Monitors markets for resolution
    - Auto-closes when outcomes determined
    - Tracks expected vs actual performance
    """
    
    def __init__(self, config: Dict, workspace_root: Path):
        self.config = config.get('paper_trading', {})
        self.full_config = config
        self.workspace_root = Path(workspace_root)
        self.logger = logging.getLogger('CompletePaperTrading')
        
        # Fee rate — read from arbitrage.fees (single source of truth)
        self.taker_fee = config.get('arbitrage', {}).get('fees', {}).get('polymarket_taker_fee', 0.0001)
        
        # Capital tracking
        self.initial_capital = self.config.get('initial_capital', 10000)
        self.current_capital = self.initial_capital
        
        # Position tracking
        self.open_positions: Dict[str, PaperPosition] = {}
        self.closed_positions: List[PaperPosition] = []
        
        # Performance tracking
        self.total_trades = 0
        self.winning_trades = 0
        self.losing_trades = 0
        self.total_expected_profit = 0.0
        self.total_actual_profit = 0.0
        self.prediction_errors: List[float] = []
        
        # Validation thresholds
        self.max_price_drift = self.config.get('max_price_drift_pct', 0.02)  # 2%
        self.max_position_age_hours = self.config.get('max_position_age_hours', 168)  # 1 week
        
        # Market data cache
        self.latest_market_data = {}
        
        # SQLite state store (in-memory + disk backup)
        self._state_store: Optional['StateStore'] = None
        self._save_counter = 0  # Track save cycles for JSON compat frequency
        if HAS_SQLITE:
            try:
                db_path = Path(workspace_root) / 'data' / 'system_state' / 'execution_state.db'
                self._state_store = StateStore(str(db_path))
                self.logger.info(f'SQLite state store initialized: {db_path}')
            except Exception as e:
                self.logger.warning(f'SQLite state store failed, using JSON fallback: {e}')
                self._state_store = None
        
        self.logger.info("Complete Paper Trading Engine initialized")
    
    async def validate_and_enter_position(
        self,
        opportunity,
        current_market_data: List
    ) -> Dict:
        """
        Step 1: Validate opportunity is still valid
        Step 2: Enter paper position if validation passes
        
        Returns: {success, reason, position}
        """
        self.logger.info(f"Validating opportunity: {opportunity.opportunity_id}")
        
        # Build current price lookup
        current_prices = {}
        for market in current_market_data:
            if market.market_id in opportunity.market_ids:
                # Get current "yes" price
                price = market.outcome_prices.get('Yes', market.outcome_prices.get('yes', market.outcome_prices.get('true', 0.5)))
                current_prices[market.market_id] = price
        
        # VALIDATION 1: Check we have current data for all markets
        if len(current_prices) != len(opportunity.market_ids):
            return {
                'success': False,
                'reason': 'missing_market_data',
                'missing': set(opportunity.market_ids) - set(current_prices.keys())
            }
        
        # VALIDATION 2: Check prices haven't drifted too much
        price_drifts = {}
        for market_id in opportunity.market_ids:
            original_price = opportunity.current_prices[market_id]
            current_price = current_prices[market_id]
            drift = abs(current_price - original_price) / original_price
            price_drifts[market_id] = drift
            
            if drift > self.max_price_drift:
                self.logger.warning(
                    f"Price drift too high for {market_id}: "
                    f"{original_price:.4f} -> {current_price:.4f} "
                    f"({drift*100:.2f}%)"
                )
                return {
                    'success': False,
                    'reason': 'price_drift_exceeded',
                    'market_id': market_id,
                    'original_price': original_price,
                    'current_price': current_price,
                    'drift_pct': drift * 100
                }
        
        # VALIDATION 3: Recalculate expected profit with current prices
        recalculated_profit = self._recalculate_profit(opportunity, current_prices)
        profit_degradation = (opportunity.expected_profit - recalculated_profit) / opportunity.expected_profit
        
        if profit_degradation > 0.2:  # More than 20% profit erosion
            self.logger.warning(
                f"Profit degradation too high: "
                f"${opportunity.expected_profit:.2f} -> ${recalculated_profit:.2f}"
            )
            return {
                'success': False,
                'reason': 'profit_degradation',
                'expected': opportunity.expected_profit,
                'recalculated': recalculated_profit,
                'degradation_pct': profit_degradation * 100
            }
        
        # VALIDATION 4: Check we have enough capital
        if opportunity.total_capital_required > self.current_capital:
            return {
                'success': False,
                'reason': 'insufficient_capital',
                'required': opportunity.total_capital_required,
                'available': self.current_capital
            }
        
        # ALL VALIDATIONS PASSED - Enter position
        position = await self._enter_position(opportunity, current_prices, price_drifts)
        
        return {
            'success': True,
            'position_id': position.position_id,
            'position': position,
            'price_drifts': price_drifts,
            'recalculated_profit': recalculated_profit
        }
    
    async def _enter_position(
        self,
        opportunity,
        current_prices: Dict[str, float],
        price_drifts: Dict[str, float]
    ) -> PaperPosition:
        """
        Actually enter the paper position using current real prices
        """
        # Build market details
        markets = {}
        for market_id, bet_amount in opportunity.optimal_bets.items():
            market_name = next(
                (name for mid, name in zip(opportunity.market_ids, opportunity.market_names) 
                 if mid == market_id),
                "Unknown"
            )
            
            markets[market_id] = {
                'name': market_name,
                'entry_price': current_prices[market_id],  # Use CURRENT price
                'bet_amount': bet_amount,
                'outcome': 'yes'  # Assuming we're buying "yes"
            }
        
        # Calculate fees on current prices
        total_cost = sum(m['bet_amount'] for m in markets.values())
        fees = total_cost * self.taker_fee
        
        # Create position
        position = PaperPosition(
            position_id=f"paper_{datetime.now().timestamp()}_{opportunity.opportunity_id}",
            opportunity_id=opportunity.opportunity_id,
            markets=markets,
            total_capital=total_cost,
            expected_profit=opportunity.expected_profit,
            expected_profit_pct=opportunity.expected_profit_pct,
            fees_paid=fees,
            entry_timestamp=datetime.now(timezone.utc),
            entry_prices=current_prices.copy(),
            status=PositionStatus.MONITORING,
            price_drift=price_drifts,
            metadata={
                'constraint_id': opportunity.constraint_id,
                'strategy': opportunity.metadata.get('strategy', 'unknown'),
                'method': opportunity.metadata.get('method', ''),
            }
        )
        
        # Update capital
        self.current_capital -= (total_cost + fees)
        
        # Track position
        self.open_positions[position.position_id] = position
        self.total_trades += 1
        self.total_expected_profit += opportunity.expected_profit
        
        self.logger.info(
            f"✓ Entered paper position {position.position_id}: "
            f"${total_cost:.2f} deployed, "
            f"expecting ${opportunity.expected_profit:.2f} profit"
        )
        
        return position
    
    def _recalculate_profit(self, opportunity, current_prices: Dict[str, float]) -> float:
        """
        Recalculate expected profit using current prices instead of original prices
        """
        # Simplified - in reality you'd re-run the arbitrage calculation
        # For now, we'll estimate based on price ratios
        
        original_sum = sum(opportunity.current_prices.values())
        current_sum = sum(current_prices.values())
        
        # If sum has gotten closer to 1.0, profit decreased
        # If sum has gotten further from 1.0, profit increased
        
        original_mispricing = abs(1.0 - original_sum)
        current_mispricing = abs(1.0 - current_sum)
        
        if original_mispricing == 0:
            return opportunity.expected_profit
        
        mispricing_ratio = current_mispricing / original_mispricing
        recalculated_profit = opportunity.expected_profit * mispricing_ratio
        
        return recalculated_profit
    
    async def monitor_positions(self, current_market_data: List):
        """
        Check all open positions for:
        1. Group resolution (ALL markets in group resolved or removed)
        2. Price drift monitoring
        No time-based expiry — capital stays locked until actual resolution.
        max_position_age_hours is used ONLY by L4 to protect near-resolution
        positions from replacement.
        """
        if not self.open_positions:
            return
        
        self.logger.debug(f"Monitoring {len(self.open_positions)} open positions")
        
        # Build market lookup
        market_lookup = {m.market_id: m for m in current_market_data}
        
        for position_id, position in list(self.open_positions.items()):
            
            # Check for GROUP resolution: all markets resolved or removed
            group_resolved, winning_market_id = self._check_group_resolved(
                position, market_lookup
            )
            
            if group_resolved and winning_market_id:
                winning_outcome = 'Yes'  # Winner has Yes >= 0.95
                self.logger.info(
                    f"Group RESOLVED for position {position_id[:40]}! "
                    f"Winner: market {winning_market_id}"
                )
                await self._close_position_on_resolution(
                    position,
                    winning_market_id,
                    winning_outcome
                )
                continue
            
            # Update price drift monitoring
            if position.status == PositionStatus.MONITORING:
                self._update_price_drift(position, market_lookup)
                position.last_check = datetime.now(timezone.utc)

            # Check for postponement / cancellation (only if not already resolved)
            if not group_resolved:
                is_postponed, reason = self._check_postponed(position, market_lookup)
                if is_postponed and not position.metadata.get('postponed'):
                    self.logger.warning(
                        f"POSTPONED detected for {position_id[:40]}: {reason}"
                    )
                    position.metadata['postponed'] = True
                    position.metadata['postponed_at'] = datetime.now(timezone.utc).isoformat()
                    position.metadata['postponed_reason'] = reason
                elif not is_postponed and position.metadata.get('postponed'):
                    # end_date extended or market rescheduled — clear flag
                    self.logger.info(
                        f"Postponement cleared for {position_id[:40]} (end_date may have been extended)"
                    )
                    position.metadata.pop('postponed', None)
                    position.metadata.pop('postponed_at', None)
                    position.metadata.pop('postponed_reason', None)

    def _check_postponed(self, position, market_lookup) -> tuple:
        """
        Detect postponed / cancelled event:
          - ALL markets in the group have end_date in the past
          - BUT none have resolved (no outcome price >= 0.95)
          - AND markets are still present in the API (not removed)
        Returns (is_postponed: bool, reason: str)
        """
        now = datetime.now(timezone.utc)
        total = len(position.markets)
        if total == 0:
            return False, ''

        past_count = 0
        for market_id in position.markets.keys():
            md = market_lookup.get(market_id)
            if md is None:
                return False, ''  # missing markets handled by _check_group_resolved
            ed = md.end_date
            if ed.tzinfo is None:
                ed = ed.replace(tzinfo=timezone.utc)
            if ed < now:
                past_count += 1

        if past_count < total:
            return False, ''  # Some markets not yet past end_date

        # All end_dates past — check none have resolved
        for market_id in position.markets.keys():
            md = market_lookup.get(market_id)
            if md is None:
                return False, ''  # let group_resolved handle this
            max_p = max((float(p) for p in md.outcome_prices.values()), default=0)
            if max_p >= 0.95:
                return False, ''  # Normal resolution in progress

        return True, (
            f'end_date passed for all {total} markets, no resolution detected '
            f'(end_dates all < {now.strftime("%Y-%m-%dT%H:%M")}Z)'
        )


    def _check_group_resolved(self, position, market_lookup) -> tuple:
        """
        Check if ALL markets in a position's group have resolved.
        A market is resolved if:
          - max(outcome_prices) >= 0.95, OR
          - market no longer in API data (removed after settlement)
        Returns (all_resolved: bool, winning_market_id: str or None)
        """
        resolved_count = 0
        missing_count = 0
        winning_market_id = None
        total_markets = len(position.markets)
        
        for market_id in position.markets.keys():
            market_data = market_lookup.get(market_id)
            
            if not market_data:
                # Market removed from API — treat as resolved
                missing_count += 1
                resolved_count += 1
                continue
            
            # Check outcome prices
            max_price = 0
            max_outcome = None
            for outcome, price in market_data.outcome_prices.items():
                p = float(price)
                if p > max_price:
                    max_price = p
                    max_outcome = outcome
            
            if max_price >= 0.95:
                resolved_count += 1
                # The winner is the market where YES >= 0.95
                if max_outcome and max_outcome.lower() == 'yes' and max_price >= 0.95:
                    winning_market_id = market_id
            
        all_resolved = resolved_count == total_markets
        
        if resolved_count > 0 and not all_resolved:
            self.logger.debug(
                f"  Partial resolution: {resolved_count}/{total_markets} "
                f"(missing={missing_count})"
            )
        
        return all_resolved, winning_market_id
    
    def _get_winning_outcome(self, market_data) -> str:
        """
        Determine which outcome won
        """
        # Find outcome with highest price (should be ~1.0 if resolved)
        max_price = 0
        winning_outcome = None
        
        for outcome, price in market_data.outcome_prices.items():
            if price > max_price:
                max_price = price
                winning_outcome = outcome
        
        return winning_outcome
    
    async def _close_position_on_resolution(
        self,
        position: PaperPosition,
        winning_market_id: str,
        winning_outcome: str
    ):
        """
        Close a paper position based on actual market resolution
        Calculate real PnL and compare to expected
        """
        # Calculate actual payout.
        # Strategy determines which formula applies:
        #
        # BUY arb (mutex_buy_all, bregman_frank_wolfe):
        #   Bought YES on all markets. Only winner pays out.
        #   bet_amount = shares_YES * YES_price → shares_YES = bet_amount / entry_price
        #   payout = shares_YES * 1.0  (winning YES shares pay $1)
        #
        # SELL arb (mutex_sell_all):
        #   Bought NO on all markets (entry_price stores YES price; NO price = 1 - entry_price).
        #   bet_amount = shares_NO * NO_price → shares_NO = bet_amount / (1 - entry_price)
        #   The WINNING market's NO bet LOSES (outcome was YES → NO payout = $0).
        #   All other markets' NO bets WIN (outcome was NO → each pays $1/share).
        #   payout = sum of (bet_amount_k / (1 - entry_price_k)) for k != winning_market
        strategy = position.metadata.get('method', '')
        is_sell_arb = 'sell' in strategy

        if is_sell_arb:
            payout = 0.0
            for market_id, mkt_data in position.markets.items():
                if market_id != winning_market_id:
                    no_price = 1.0 - mkt_data['entry_price']
                    if no_price > 0:
                        shares_no = mkt_data['bet_amount'] / no_price
                        payout += shares_no  # Each NO share pays $1
        else:
            winning_market = position.markets[winning_market_id]
            entry_price = winning_market['entry_price']
            bet_amount = winning_market['bet_amount']
            shares_bought = bet_amount / entry_price
            payout = shares_bought * 1.00  # Winning YES shares pay $1
        
        # Calculate profit
        total_invested = position.total_capital + position.fees_paid
        actual_profit = payout - total_invested
        actual_profit_pct = actual_profit / total_invested if total_invested > 0 else 0
        
        # Update position
        position.status = PositionStatus.CLOSED
        position.resolved_at = datetime.now(timezone.utc)
        position.close_timestamp = time.time()  # For deduplication
        position.winning_market = winning_market_id
        position.actual_payout = payout
        position.actual_profit = actual_profit
        position.actual_profit_pct = actual_profit_pct
        position.profit_delta = actual_profit - position.expected_profit
        position.profit_accuracy = actual_profit / position.expected_profit if position.expected_profit != 0 else 0
        position.metadata['close_reason'] = 'resolved'
        
        # Update capital
        self.current_capital += payout
        
        # Track performance
        self.total_actual_profit += actual_profit
        
        if actual_profit > 0:
            self.winning_trades += 1
        else:
            self.losing_trades += 1
        
        # Track prediction accuracy
        prediction_error = abs(position.profit_delta) / abs(position.expected_profit) if position.expected_profit != 0 else 0
        self.prediction_errors.append(prediction_error)
        
        # Move to closed positions
        del self.open_positions[position.position_id]
        self.closed_positions.append(position)
        
        self.logger.info(
            f"✓ Position {position.position_id} CLOSED\n"
            f"  Expected profit: ${position.expected_profit:.2f}\n"
            f"  Actual profit:   ${actual_profit:.2f}\n"
            f"  Delta:           ${position.profit_delta:.2f}\n"
            f"  Accuracy:        {position.profit_accuracy*100:.1f}%\n"
            f"  New capital:     ${self.current_capital:.2f}"
        )
    
    # _expire_position REMOVED - positions close only on actual resolution
    # max_position_age_hours is used solely by L4 to protect near-resolution
    # positions from replacement, NOT to expire/close positions.

    async def liquidate_position(self, position_id: str, market_lookup: Dict) -> Dict:
        """
        Close a position early by selling at current market prices.
        Returns dict with liquidation details.
        """
        if position_id not in self.open_positions:
            return {'success': False, 'reason': 'position_not_found'}
        
        position = self.open_positions[position_id]
        
        # Calculate liquidation value: for each market, we hold shares = bet_amount/entry_price
        # Current value of those shares = shares * current_price
        liquidation_value = 0.0
        for market_id, mkt_info in position.markets.items():
            entry_price = mkt_info.get('entry_price', 0)
            bet_amount = mkt_info.get('bet_amount', 0)
            if entry_price <= 0:
                continue
            shares = bet_amount / entry_price
            # Get current price
            mkt_data = market_lookup.get(str(market_id))
            if mkt_data:
                outcome = mkt_info.get('outcome', 'Yes')
                if hasattr(mkt_data, 'outcome_prices'):
                    current_price = mkt_data.outcome_prices.get(outcome, entry_price)
                else:
                    current_price = entry_price
            else:
                current_price = entry_price  # fallback
            liquidation_value += shares * current_price
        
        # Apply exit fee (same as entry fee)
        fee_rate = self.taker_fee
        exit_fee = liquidation_value * fee_rate
        net_liquidation = liquidation_value - exit_fee
        
        actual_profit = net_liquidation - position.total_capital
        
        # Close the position
        position.status = PositionStatus.CLOSED
        position.resolved_at = datetime.now(timezone.utc)
        position.close_timestamp = time.time()
        position.actual_payout = net_liquidation
        position.actual_profit = actual_profit
        position.actual_profit_pct = actual_profit / position.total_capital if position.total_capital > 0 else 0
        position.profit_delta = actual_profit - position.expected_profit
        position.profit_accuracy = actual_profit / position.expected_profit if position.expected_profit != 0 else 0
        position.metadata['close_reason'] = 'replaced'
        
        self.current_capital += net_liquidation
        self.total_actual_profit += actual_profit
        if actual_profit > 0:
            self.winning_trades += 1
        else:
            self.losing_trades += 1
        self.prediction_errors.append(abs(actual_profit - position.expected_profit))
        
        del self.open_positions[position_id]
        self.closed_positions.append(position)
        
        self.logger.info(
            f"✓ Position {position_id} LIQUIDATED (replaced)\n"
            f"  Capital deployed:   ${position.total_capital:.2f}\n"
            f"  Liquidation value:  ${net_liquidation:.2f}\n"
            f"  Realized P&L:       ${actual_profit:+.2f}\n"
            f"  New capital:        ${self.current_capital:.2f}"
        )
        
        return {
            'success': True,
            'liquidation_value': net_liquidation,
            'actual_profit': actual_profit,
            'freed_capital': net_liquidation
        }

    def _update_price_drift(self, position: PaperPosition, market_lookup: Dict):
        """
        Update price drift tracking for monitoring
        """
        for market_id, entry_price in position.entry_prices.items():
            market_data = market_lookup.get(market_id)
            if market_data:
                current_price = market_data.outcome_prices.get('yes', 0.5)
                drift = abs(current_price - entry_price) / entry_price
                position.price_drift[market_id] = drift
    
    def get_performance_metrics(self) -> Dict:
        """
        Get comprehensive performance metrics
        """
        win_rate = self.winning_trades / self.total_trades if self.total_trades > 0 else 0
        
        avg_prediction_error = sum(self.prediction_errors) / len(self.prediction_errors) if self.prediction_errors else 0
        
        total_return = (self.current_capital - self.initial_capital) / self.initial_capital
        
        return {
            'current_capital': self.current_capital,
            'initial_capital': self.initial_capital,
            'total_return': total_return,
            'total_return_pct': total_return * 100,
            
            'total_trades': self.total_trades,
            'winning_trades': self.winning_trades,
            'losing_trades': self.losing_trades,
            'win_rate': win_rate,
            
            'open_positions': len(self.open_positions),
            'closed_positions': len(self.closed_positions),
            
            'expected_total_profit': self.total_expected_profit,
            'actual_total_profit': self.total_actual_profit,
            'profit_accuracy': self.total_actual_profit / self.total_expected_profit if self.total_expected_profit != 0 else 0,
            'avg_prediction_error_pct': avg_prediction_error * 100,
            
            'timestamp': datetime.now(timezone.utc).isoformat()
        }
    
    def save_state(self, output_path: Path):
        """Save complete state — SQLite primary, JSON for backward compat."""
        output_path.parent.mkdir(parents=True, exist_ok=True)
        
        if self._state_store:
            try:
                import time as _t
                t0 = _t.time()
                # Sync scalars
                self._state_store.save_scalars({
                    'current_capital': self.current_capital,
                    'initial_capital': self.initial_capital,
                })
                # Sync performance metrics as scalars
                perf = self.get_performance_metrics()
                for k in ('total_trades', 'winning_trades', 'losing_trades',
                          'total_actual_profit', 'total_expected_profit'):
                    if k in perf:
                        self._state_store.save_scalar(k, perf[k])
                
                # Sync open positions (full replace — small set, always fresh)
                open_dicts = [p.to_dict() for p in self.open_positions.values()]
                # Delete any positions in DB that are no longer open
                db_open_ids = {p['position_id'] for p in self._state_store.get_positions_by_status('open')}
                db_open_ids |= {p['position_id'] for p in self._state_store.get_positions_by_status('monitoring')}
                live_open_ids = {p.position_id for p in self.open_positions.values()}
                for stale_id in (db_open_ids - live_open_ids):
                    self._state_store.delete_position(stale_id)
                if open_dicts:
                    self._state_store.upsert_positions_bulk(open_dicts, 'open')
                
                # Sync closed positions (incremental — only new ones)
                db_closed_count = len(self._state_store.get_positions_by_status('closed'))
                if len(self.closed_positions) > db_closed_count:
                    new_closed = [p.to_dict() for p in self.closed_positions[db_closed_count:]]
                    self._state_store.upsert_positions_bulk(new_closed, 'closed')
                
                # Atomic backup to disk
                self._state_store.backup_to_disk()
                
                # JSON compat for dashboard — every 5th save (~150s at 30s interval)
                self._save_counter += 1
                if self._save_counter % 5 == 1:
                    self._state_store.save_json_compat(str(output_path))
                
                elapsed_ms = (_t.time() - t0) * 1000
                self.logger.info(f"Saved state (SQLite): {len(self.open_positions)} open, "
                                f"{len(self.closed_positions)} closed [{elapsed_ms:.0f}ms]")
                return
            except Exception as e:
                self.logger.warning(f"SQLite save failed, falling back to JSON: {e}")
        
        # JSON fallback
        state = {
            'current_capital': self.current_capital,
            'initial_capital': self.initial_capital,
            'open_positions': [p.to_dict() for p in self.open_positions.values()],
            'closed_positions': [p.to_dict() for p in self.closed_positions],
            'performance': self.get_performance_metrics()
        }
        
        with open(output_path, 'w') as f:
            json.dump(state, f, separators=(',', ':'))
        
        self.logger.info(f"Saved state (JSON): {len(self.open_positions)} open, {len(self.closed_positions)} closed")



    def load_state(self, state_path: Path):
        """Load state — SQLite primary, JSON migration fallback."""
        
        # Try SQLite first
        if self._state_store:
            try:
                restored = self._state_store.restore_from_disk()
                if restored:
                    self._load_from_state_store()
                    self.logger.info(f"Loaded state (SQLite): capital=${self.current_capital:.2f}, "
                                    f"{len(self.open_positions)} open, {len(self.closed_positions)} closed")
                    return
                # No disk DB yet — try importing from JSON
                if state_path.exists():
                    self.logger.info(f"No SQLite state, migrating from JSON: {state_path}")
                    self._state_store.import_from_json(str(state_path))
                    self._load_from_state_store()
                    self._state_store.backup_to_disk()  # Persist the migration
                    self.logger.info(f"Migrated JSON→SQLite: capital=${self.current_capital:.2f}, "
                                    f"{len(self.open_positions)} open, {len(self.closed_positions)} closed")
                    return
            except Exception as e:
                self.logger.warning(f"SQLite load failed, falling back to JSON: {e}")
        
        # JSON fallback
        with open(state_path) as f:
            state = json.load(f)
        self._load_from_json_dict(state)
    
    def _load_from_state_store(self):
        """Populate in-memory structures from SQLite state store."""
        scalars = self._state_store.get_all_scalars()
        self.current_capital = scalars.get('current_capital', self.initial_capital)
        self.initial_capital = scalars.get('initial_capital', self.initial_capital)
        
        # Restore open positions
        self.open_positions = {}
        for p_dict in self._state_store.get_positions_by_status('open'):
            pos = self._dict_to_position(p_dict)
            self.open_positions[pos.position_id] = pos
        # Also load monitoring positions as open
        for p_dict in self._state_store.get_positions_by_status('monitoring'):
            pos = self._dict_to_position(p_dict)
            self.open_positions[pos.position_id] = pos
        
        # Restore closed positions
        self.closed_positions = []
        for p_dict in self._state_store.get_positions_by_status('closed'):
            self.closed_positions.append(self._dict_to_position(p_dict))
        
        # Restore metrics
        self.total_trades = int(scalars.get('total_trades', 0))
        self.winning_trades = int(scalars.get('winning_trades', 0))
        self.losing_trades = int(scalars.get('losing_trades', 0))
        self.total_actual_profit = scalars.get('total_actual_profit', 0)
        self.total_expected_profit = scalars.get('total_expected_profit', 0)
    
    def _dict_to_position(self, p_dict: dict) -> PaperPosition:
        """Convert a dict back to PaperPosition."""
        return PaperPosition(
            position_id=p_dict.get('position_id', ''),
            opportunity_id=p_dict.get('opportunity_id', ''),
            markets=p_dict.get('markets', {}),
            total_capital=p_dict.get('total_capital', 0),
            expected_profit=p_dict.get('expected_profit', 0),
            expected_profit_pct=p_dict.get('expected_profit_pct', 0),
            fees_paid=p_dict.get('fees_paid', 0),
            entry_timestamp=datetime.fromisoformat(p_dict['entry_timestamp']) if p_dict.get('entry_timestamp') else datetime.now(timezone.utc),
            entry_prices=p_dict.get('entry_prices', {}),
            status=PositionStatus(p_dict.get('status', 'open')),
            close_timestamp=p_dict.get('close_timestamp'),
            actual_profit=p_dict.get('actual_profit', 0),
            actual_profit_pct=p_dict.get('actual_profit_pct', 0),
            actual_payout=p_dict.get('actual_payout', 0),
            winning_market=p_dict.get('winning_market'),
            resolved_at=datetime.fromisoformat(p_dict['resolved_at']) if p_dict.get('resolved_at') else None,
            profit_delta=p_dict.get('profit_delta', 0),
            profit_accuracy=p_dict.get('profit_accuracy', 0),
            metadata=p_dict.get('metadata', {}),
        )
    
    def _load_from_json_dict(self, state: dict):
        """Load from a parsed JSON state dict (original load_state logic)."""
        self.current_capital = state.get('current_capital', self.initial_capital)
        self.initial_capital = state.get('initial_capital', self.initial_capital)
        
        # Restore closed positions as dicts (we don't reconstruct PaperPosition)
        self.closed_positions = []
        for p_dict in state.get('closed_positions', []):
            if isinstance(p_dict, dict):
                pos = PaperPosition(
                    position_id=p_dict.get('position_id', ''),
                    opportunity_id=p_dict.get('opportunity_id', ''),
                    markets=p_dict.get('markets', {}),
                    total_capital=p_dict.get('total_capital', 0),
                    expected_profit=p_dict.get('expected_profit', 0),
                    expected_profit_pct=p_dict.get('expected_profit_pct', 0),
                    fees_paid=p_dict.get('fees_paid', 0),
                    entry_timestamp=datetime.fromisoformat(p_dict['entry_timestamp']) if p_dict.get('entry_timestamp') else datetime.now(timezone.utc),
                    entry_prices=p_dict.get('entry_prices', {}),
                    status=PositionStatus(p_dict.get('status', 'closed')),
                    close_timestamp=p_dict.get('close_timestamp'),
                    actual_profit=p_dict.get('actual_profit', 0),
                    actual_profit_pct=p_dict.get('actual_profit_pct', 0),
                    metadata=p_dict.get('metadata', {}),
                )
                self.closed_positions.append(pos)
        
        # Restore open positions
        self.open_positions = {}
        for p_dict in state.get('open_positions', []):
            if isinstance(p_dict, dict):
                pos = PaperPosition(
                    position_id=p_dict.get('position_id', ''),
                    opportunity_id=p_dict.get('opportunity_id', ''),
                    markets=p_dict.get('markets', {}),
                    total_capital=p_dict.get('total_capital', 0),
                    expected_profit=p_dict.get('expected_profit', 0),
                    expected_profit_pct=p_dict.get('expected_profit_pct', 0),
                    fees_paid=p_dict.get('fees_paid', 0),
                    entry_timestamp=datetime.fromisoformat(p_dict['entry_timestamp']) if p_dict.get('entry_timestamp') else datetime.now(timezone.utc),
                    entry_prices=p_dict.get('entry_prices', {}),
                    status=PositionStatus(p_dict.get('status', 'open')),
                    metadata=p_dict.get('metadata', {}),
                )
                self.open_positions[pos.position_id] = pos
        
        # Restore metrics
        perf = state.get('performance', {})
        self.total_trades = perf.get('total_trades', 0)
        self.winning_trades = perf.get('winning_trades', 0)
        self.losing_trades = perf.get('losing_trades', 0)
        self.total_actual_profit = perf.get('total_actual_profit', 0)
        self.total_expected_profit = perf.get('total_expected_profit', 0)
        
        self.logger.info(f"Loaded state: capital=${self.current_capital:.2f}, "
                         f"{len(self.open_positions)} open, {len(self.closed_positions)} closed")


    async def execute_opportunity(self, opp_dict: Dict, markets: List) -> Optional[Dict]:
        """Wrapper: convert dict opportunity to expected format and execute"""
        from types import SimpleNamespace
        
        # Convert dict to object with attributes
        opp = SimpleNamespace(
            opportunity_id=opp_dict.get('opportunity_id', ''),
            constraint_id=opp_dict.get('constraint_id', ''),
            market_ids=opp_dict.get('market_ids', []),
            market_names=opp_dict.get('market_names', []),
            current_prices=opp_dict.get('current_prices', {}),
            optimal_bets=opp_dict.get('optimal_bets', {}),
            expected_profit=opp_dict.get('expected_profit', 0),
            expected_profit_pct=opp_dict.get('expected_profit_pct', 0),
            max_loss=opp_dict.get('max_loss', 0),
            worst_case_return=opp_dict.get('worst_case_return', 0),
            total_capital_required=opp_dict.get('total_capital_required', 100),
            fees_estimated=opp_dict.get('fees_estimated', 0),
            net_profit=opp_dict.get('net_profit', 0),
            metadata=opp_dict.get('metadata', {}),
        )
        
        return await self.validate_and_enter_position(opp, markets)

if __name__ == '__main__':
    # Test
    logging.basicConfig(level=logging.INFO)
    
    async def test():
        engine = PaperTradingEngine(
            {'paper_trading': {'initial_capital': 10000}},
            Path('../')
        )
        
        print(f"Initial capital: ${engine.current_capital}")
        print("\nTesting validation and entry...")
        
        # Mock opportunity and market data
        from dataclasses import dataclass
        
        @dataclass
        class MockOpportunity:
            opportunity_id = "test_001"
            constraint_id = "constraint_1"
            market_ids = ["market_1", "market_2"]
            market_names = ["Market 1", "Market 2"]
            optimal_bets = {"market_1": 50, "market_2": 50}
            current_prices = {"market_1": 0.40, "market_2": 0.55}
            total_capital_required = 100
            expected_profit = 5.0
            expected_profit_pct = 0.05
            metadata = {'strategy': 'test'}
        
        @dataclass
        class MockMarket:
            market_id: str
            market_name: str
            outcome_prices: dict
            end_date: datetime
        
        markets = [
            MockMarket("market_1", "M1", {'yes': 0.40, 'no': 0.60}, datetime.now(timezone.utc) + timedelta(days=7)),
            MockMarket("market_2", "M2", {'yes': 0.55, 'no': 0.45}, datetime.now(timezone.utc) + timedelta(days=7))
        ]
        
        result = await engine.validate_and_enter_position(MockOpportunity(), markets)
        print(f"\nEntry result: {result['success']}")
        if result['success']:
            print(f"Position ID: {result['position_id']}")
            print(f"Price drifts: {result['price_drifts']}")
        
        print(f"\nMetrics:")
        metrics = engine.get_performance_metrics()
        for key, value in metrics.items():
            print(f"  {key}: {value}")
    
    asyncio.run(test())
