"""
Layer 4: Execution Engine
The Hands - places trades and manages positions
"""

import logging
import asyncio
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import Dict, List, Optional
from enum import Enum
from pathlib import Path
import json


class OrderStatus(Enum):
    PENDING = "pending"
    FILLED = "filled"
    PARTIALLY_FILLED = "partially_filled"
    CANCELLED = "cancelled"
    FAILED = "failed"


class OrderType(Enum):
    MARKET = "market"
    LIMIT = "limit"


@dataclass
class Order:
    """Represents a trade order"""
    order_id: str
    market_id: str
    market_name: str
    order_type: OrderType
    side: str  # "buy" or "sell"
    amount: float  # USD amount
    price: Optional[float]  # Limit price (None for market orders)
    status: OrderStatus = OrderStatus.PENDING
    filled_amount: float = 0.0
    filled_price: Optional[float] = None
    created_at: datetime = field(default_factory=lambda: datetime.now(timezone.utc))
    filled_at: Optional[datetime] = None
    metadata: Dict = field(default_factory=dict)
    
    def to_dict(self) -> Dict:
        return {
            'order_id': self.order_id,
            'market_id': self.market_id,
            'market_name': self.market_name,
            'order_type': self.order_type.value,
            'side': self.side,
            'amount': self.amount,
            'price': self.price,
            'status': self.status.value,
            'filled_amount': self.filled_amount,
            'filled_price': self.filled_price,
            'created_at': self.created_at.isoformat(),
            'filled_at': self.filled_at.isoformat() if self.filled_at else None,
            'metadata': self.metadata
        }


@dataclass
class Position:
    """Represents an open position"""
    position_id: str
    market_id: str
    market_name: str
    quantity: float
    entry_price: float
    current_price: float
    unrealized_pnl: float
    realized_pnl: float = 0.0
    opened_at: datetime = field(default_factory=lambda: datetime.now(timezone.utc))
    closed_at: Optional[datetime] = None
    
    def update_price(self, new_price: float):
        """Update current price and recalculate PnL"""
        self.current_price = new_price
        self.unrealized_pnl = (new_price - self.entry_price) * self.quantity
    
    def to_dict(self) -> Dict:
        return {
            'position_id': self.position_id,
            'market_id': self.market_id,
            'market_name': self.market_name,
            'quantity': self.quantity,
            'entry_price': self.entry_price,
            'current_price': self.current_price,
            'unrealized_pnl': self.unrealized_pnl,
            'realized_pnl': self.realized_pnl,
            'opened_at': self.opened_at.isoformat(),
            'closed_at': self.closed_at.isoformat() if self.closed_at else None
        }


class PaperTradingEngine:
    """
    Simulates trading without real money.
    Critical for testing before going live.
    """
    
    def __init__(self, config: Dict, workspace_root: Path):
        self.config = config.get('paper_trading', {})
        self.workspace_root = Path(workspace_root)
        self.logger = logging.getLogger('PaperTradingEngine')
        
        # Paper trading state
        self.initial_capital = self.config.get('initial_capital', 10000)
        self.current_capital = self.initial_capital
        self.positions: Dict[str, Position] = {}
        self.orders: List[Order] = []
        self.trade_history: List[Dict] = []
        
        # Performance tracking
        self.total_trades = 0
        self.winning_trades = 0
        self.losing_trades = 0
        self.total_profit = 0.0
        self.total_loss = 0.0
        self.max_drawdown = 0.0
        self.peak_capital = self.initial_capital
        
        # Simulation parameters
        self.simulated_latency = self.config.get('simulated_latency', 0.5)
        self.simulated_fill_rate = self.config.get('simulated_fill_rate', 0.95)
        self.trading_fee = config.get('arbitrage', {}).get('fees', {}).get('trading_fee', 0.0001)
    
    async def execute_arbitrage(self, opportunity) -> Dict:
        """
        Execute an arbitrage opportunity in paper trading mode
        
        Returns: Execution report
        """
        self.logger.info(f"Paper trading arbitrage: {opportunity.opportunity_id}")
        
        # Check if we have enough capital
        required_capital = opportunity.total_capital_required
        if required_capital > self.current_capital:
            self.logger.warning(f"Insufficient capital: need ${required_capital}, have ${self.current_capital}")
            return {
                'success': False,
                'reason': 'insufficient_capital',
                'required': required_capital,
                'available': self.current_capital
            }
        
        # Simulate placing orders for each market
        orders_placed = []
        total_cost = 0.0
        
        for market_id, bet_amount in opportunity.optimal_bets.items():
            market_name = next(
                (name for mid, name in zip(opportunity.market_ids, opportunity.market_names) 
                 if mid == market_id),
                "Unknown"
            )
            
            price = opportunity.current_prices[market_id]
            
            # Create paper order
            order = await self._place_paper_order(
                market_id=market_id,
                market_name=market_name,
                amount=bet_amount,
                price=price,
                side="buy"
            )
            
            orders_placed.append(order)
            
            if order.status == OrderStatus.FILLED:
                total_cost += order.filled_amount
        
        # Update capital
        fees = total_cost * self.trading_fee
        self.current_capital -= (total_cost + fees)
        
        # Track the arbitrage position
        arb_position = {
            'opportunity_id': opportunity.opportunity_id,
            'markets': opportunity.market_ids,
            'capital_deployed': total_cost,
            'fees_paid': fees,
            'expected_profit': opportunity.expected_profit,
            'orders': [o.to_dict() for o in orders_placed],
            'executed_at': datetime.now(timezone.utc).isoformat(),
            'status': 'open'
        }
        
        self.trade_history.append(arb_position)
        
        return {
            'success': True,
            'orders_filled': len([o for o in orders_placed if o.status == OrderStatus.FILLED]),
            'total_cost': total_cost,
            'fees': fees,
            'capital_remaining': self.current_capital,
            'position': arb_position
        }
    
    async def _place_paper_order(
        self,
        market_id: str,
        market_name: str,
        amount: float,
        price: float,
        side: str
    ) -> Order:
        """Simulate placing an order"""
        
        order = Order(
            order_id=f"paper_{datetime.now().timestamp()}_{market_id}",
            market_id=market_id,
            market_name=market_name,
            order_type=OrderType.LIMIT,
            side=side,
            amount=amount,
            price=price
        )
        
        # Simulate network latency
        await asyncio.sleep(self.simulated_latency)
        
        # Simulate fill rate
        import random
        if random.random() < self.simulated_fill_rate:
            # Order filled
            order.status = OrderStatus.FILLED
            order.filled_amount = amount
            order.filled_price = price
            order.filled_at = datetime.now(timezone.utc)
            
            self.logger.debug(f"Paper order filled: {market_name} ${amount:.2f} @ {price:.4f}")
        else:
            # Order failed to fill
            order.status = OrderStatus.FAILED
            self.logger.warning(f"Paper order failed: {market_name}")
        
        self.orders.append(order)
        self.total_trades += 1
        
        return order
    
    def simulate_outcome(self, market_id: str, winning: bool) -> float:
        """
        Simulate market resolution
        
        Returns: Profit/loss from this market
        """
        relevant_orders = [o for o in self.orders if o.market_id == market_id and o.status == OrderStatus.FILLED]
        
        pnl = 0.0
        
        for order in relevant_orders:
            if winning:
                # Winning bet pays out 1.0 per dollar at price
                payout = order.filled_amount / order.filled_price
                profit = payout - order.filled_amount
                pnl += profit
                self.winning_trades += 1
            else:
                # Losing bet loses the amount invested
                loss = order.filled_amount
                pnl -= loss
                self.losing_trades += 1
        
        return pnl
    
    def close_arbitrage_position(self, opportunity_id: str, outcome_market_id: str) -> Dict:
        """
        Close an arbitrage position based on which market won
        
        In a proper arbitrage, we profit regardless, but we track which market actually won
        """
        # Find the position
        position = next((p for p in self.trade_history if p['opportunity_id'] == opportunity_id), None)
        
        if not position or position['status'] != 'open':
            return {'success': False, 'reason': 'position_not_found'}
        
        # Calculate PnL
        # In a true arbitrage, exactly one market wins and we profit
        markets = position['markets']
        capital_deployed = position['capital_deployed']
        fees_paid = position['fees_paid']
        
        # The winning market pays out
        winning_order = next(
            (o for o in position['orders'] if o['market_id'] == outcome_market_id and o['status'] == 'filled'),
            None
        )
        
        if winning_order:
            payout = winning_order['filled_amount'] / winning_order['filled_price']
            realized_pnl = payout - capital_deployed - fees_paid
        else:
            # Edge case: the winning market wasn't in our arbitrage
            realized_pnl = -capital_deployed - fees_paid
        
        # Update capital
        self.current_capital += (capital_deployed + realized_pnl)
        
        # Track performance
        if realized_pnl > 0:
            self.total_profit += realized_pnl
        else:
            self.total_loss += abs(realized_pnl)
        
        # Update drawdown
        if self.current_capital > self.peak_capital:
            self.peak_capital = self.current_capital
        else:
            drawdown = (self.peak_capital - self.current_capital) / self.peak_capital
            if drawdown > self.max_drawdown:
                self.max_drawdown = drawdown
        
        # Mark position as closed
        position['status'] = 'closed'
        position['closed_at'] = datetime.now(timezone.utc).isoformat()
        position['realized_pnl'] = realized_pnl
        position['winning_market'] = outcome_market_id
        
        self.logger.info(f"Closed arbitrage {opportunity_id}: PnL ${realized_pnl:.2f}")
        
        return {
            'success': True,
            'realized_pnl': realized_pnl,
            'new_capital': self.current_capital,
            'position': position
        }
    
    def get_performance_metrics(self) -> Dict:
        """Get current performance statistics"""
        
        win_rate = (self.winning_trades / self.total_trades) if self.total_trades > 0 else 0
        avg_profit = (self.total_profit / self.winning_trades) if self.winning_trades > 0 else 0
        avg_loss = (self.total_loss / self.losing_trades) if self.losing_trades > 0 else 0
        
        total_return = (self.current_capital - self.initial_capital) / self.initial_capital
        
        return {
            'current_capital': self.current_capital,
            'initial_capital': self.initial_capital,
            'total_return': total_return,
            'total_return_pct': total_return * 100,
            'total_profit': self.total_profit,
            'total_loss': self.total_loss,
            'net_pnl': self.total_profit - self.total_loss,
            'total_trades': self.total_trades,
            'winning_trades': self.winning_trades,
            'losing_trades': self.losing_trades,
            'win_rate': win_rate,
            'avg_profit_per_win': avg_profit,
            'avg_loss_per_loss': avg_loss,
            'max_drawdown': self.max_drawdown,
            'max_drawdown_pct': self.max_drawdown * 100,
            'open_positions': len([p for p in self.trade_history if p['status'] == 'open']),
            'timestamp': datetime.now(timezone.utc).isoformat()
        }
    
    def save_state(self, output_path: Path):
        """Save paper trading state to disk"""
        output_path.parent.mkdir(parents=True, exist_ok=True)
        
        state = {
            'config': self.config,
            'current_capital': self.current_capital,
            'positions': [p.to_dict() for p in self.positions.values()],
            'orders': [o.to_dict() for o in self.orders],
            'trade_history': self.trade_history,
            'performance': self.get_performance_metrics()
        }
        
        with open(output_path, 'w') as f:
            json.dump(state, f, indent=2)
        
        self.logger.info(f"Saved paper trading state to {output_path}")
    
    def load_state(self, input_path: Path):
        """Load paper trading state from disk"""
        with open(input_path, 'r') as f:
            state = json.load(f)
        
        self.current_capital = state['current_capital']
        self.trade_history = state['trade_history']
        
        # Reconstruct performance metrics
        perf = state.get('performance', {})
        self.total_trades = perf.get('total_trades', 0)
        self.winning_trades = perf.get('winning_trades', 0)
        self.losing_trades = perf.get('losing_trades', 0)
        self.total_profit = perf.get('total_profit', 0)
        self.total_loss = perf.get('total_loss', 0)
        self.max_drawdown = perf.get('max_drawdown', 0)
        
        self.logger.info(f"Loaded paper trading state from {input_path}")


if __name__ == '__main__':
    # Test
    import asyncio
    logging.basicConfig(level=logging.INFO)
    
    async def test():
        engine = PaperTradingEngine(
            {'paper_trading': {'initial_capital': 10000}},
            Path('../')
        )
        
        print(f"Initial capital: ${engine.current_capital}")
        
        # Mock arbitrage opportunity
        from dataclasses import dataclass
        from datetime import datetime
        
        @dataclass
        class MockOpportunity:
            opportunity_id = "test_001"
            market_ids = ["market_1", "market_2"]
            market_names = ["Market 1", "Market 2"]
            optimal_bets = {"market_1": 50, "market_2": 50}
            current_prices = {"market_1": 0.4, "market_2": 0.55}
            total_capital_required = 100
            expected_profit = 5.0
        
        result = await engine.execute_arbitrage(MockOpportunity())
        print(f"\nExecution result: {result}")
        
        print(f"\nPerformance metrics:")
        metrics = engine.get_performance_metrics()
        for key, value in metrics.items():
            print(f"  {key}: {value}")
    
    asyncio.run(test())
