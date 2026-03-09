"""
Layer 3: Arbitrage Math Engine
The Calculator - finds guaranteed profit opportunities using mathematical optimization

Advanced maths implemented:
  1. Marginal polytope construction  - defines all feasible outcome scenarios
  2. Bregman projection (KL)         - finds closest consistent distribution to observed prices
  3. Frank-Wolfe optimization        - efficient LP solving for multi-market bet sizing
"""

import logging
import numpy as np
from dataclasses import dataclass
from typing import List, Dict, Tuple, Optional
from datetime import datetime
from pathlib import Path
import json
from itertools import product as iterproduct

try:
    import cvxpy as cp
    HAS_CVXPY = True
except ImportError:
    HAS_CVXPY = False
    logging.warning("CVXPY not installed. Using simplified math only.")


@dataclass
class ArbitrageOpportunity:
    """Represents a detected arbitrage opportunity"""
    opportunity_id: str
    constraint_id: str
    market_ids: List[str]
    market_names: List[str]
    current_prices: Dict[str, float]
    optimal_bets: Dict[str, float]
    expected_profit: float
    expected_profit_pct: float
    max_loss: float
    worst_case_return: float
    total_capital_required: float
    fees_estimated: float
    net_profit: float
    detected_at: datetime
    expires_at: Optional[datetime]
    metadata: Dict

    def to_dict(self) -> Dict:
        return {
            'opportunity_id': self.opportunity_id,
            'constraint_id': self.constraint_id,
            'market_ids': self.market_ids,
            'market_names': self.market_names,
            'current_prices': self.current_prices,
            'optimal_bets': self.optimal_bets,
            'expected_profit': self.expected_profit,
            'expected_profit_pct': self.expected_profit_pct,
            'max_loss': self.max_loss,
            'worst_case_return': self.worst_case_return,
            'total_capital_required': self.total_capital_required,
            'fees_estimated': self.fees_estimated,
            'net_profit': self.net_profit,
            'detected_at': self.detected_at.isoformat(),
            'expires_at': self.expires_at.isoformat() if self.expires_at else None,
            'metadata': self.metadata
        }


class MarginalPolytope:
    """
    Constructs the marginal polytope for a set of related prediction markets.

    The marginal polytope M is the convex hull of all feasible outcome vectors.
    Each vertex is a valid joint outcome (e.g. which markets resolve YES).
    Prices that lie outside M represent an arbitrage — they imply contradictory
    probabilities, and a risk-free profit can be extracted.

    For N binary markets with constraints, we:
      1. Enumerate all 2^N binary outcome vectors
      2. Filter to those that satisfy all constraints (mutex, implication, complement)
      3. The remaining vectors are the extreme points of the polytope
    """

    def __init__(self, market_ids: List[str], constraint_type: str,
                 implications: List[Tuple[int, int]] = None):
        self.market_ids = market_ids
        self.n = len(market_ids)
        self.constraint_type = constraint_type
        self.implications = implications or []
        self.valid_scenarios = self._enumerate_valid_scenarios()

    def _enumerate_valid_scenarios(self) -> np.ndarray:
        """
        Enumerate all binary outcome vectors consistent with constraints.
        Returns array of shape (S, N) where S = number of valid scenarios.
        """
        from layer2_constraint_detection.constraint_detector import RelationshipType

        all_outcomes = np.array(list(iterproduct([0, 1], repeat=self.n)))
        valid = []

        for outcome in all_outcomes:
            if self._satisfies_constraints(outcome):
                valid.append(outcome)

        if not valid:
            # Fallback: at least one market wins (for mutex case)
            valid = [np.eye(self.n, dtype=int)[i] for i in range(self.n)]

        return np.array(valid, dtype=float)

    def _satisfies_constraints(self, outcome: np.ndarray) -> bool:
        """Check if an outcome vector satisfies all constraints."""
        from layer2_constraint_detection.constraint_detector import RelationshipType

        ct = self.constraint_type

        if ct == RelationshipType.MUTUAL_EXCLUSIVITY or ct == 'mutual_exclusivity':
            # Exactly one market resolves YES
            return outcome.sum() == 1

        elif ct == RelationshipType.COMPLEMENTARY or ct == 'complementary':
            # For 2 markets: exactly one YES
            if self.n == 2:
                return outcome.sum() == 1
            return outcome.sum() <= 1

        elif ct == RelationshipType.LOGICAL_IMPLICATION or ct == 'logical_implication':
            # Each implication (i -> j): if outcome[i]=1 then outcome[j]=1
            for (i, j) in self.implications:
                if outcome[i] == 1 and outcome[j] == 0:
                    return False
            # Also: at least one outcome can happen
            return True

        else:
            # Unknown: accept all
            return True

    def contains(self, prices: np.ndarray) -> bool:
        """
        Check if the price vector lies within the marginal polytope.
        A price vector p is in M iff there exists a convex combination of
        valid scenarios that equals p (i.e. p = sum lambda_s * s, lambda >= 0, sum=1).
        We solve this as a small LP.
        """
        if not HAS_CVXPY or len(self.valid_scenarios) == 0:
            return True

        S = len(self.valid_scenarios)
        lam = cp.Variable(S, nonneg=True)
        constraints = [
            cp.sum(lam) == 1,
            self.valid_scenarios.T @ lam == prices
        ]
        prob = cp.Problem(cp.Minimize(0), constraints)
        try:
            prob.solve(solver=cp.ECOS, verbose=False)
            return prob.status in ('optimal', 'optimal_inaccurate')
        except Exception:
            return True  # Assume feasible on solver failure


def bregman_project_kl(prices: np.ndarray, polytope: MarginalPolytope,
                        max_iter: int = 500, tol: float = 1e-8) -> np.ndarray:
    """
    Bregman projection of observed prices onto the marginal polytope using
    KL divergence (I-projection):

        q* = argmin_{q in M}  KL(q || p)
           = argmin_{q in M}  sum_i  q_i * log(q_i / p_i)

    q* is the closest consistent distribution to the observed market prices p.
    Markets where p_i < q*_i are underpriced  -> buy
    Markets where p_i > q*_i are overpriced   -> sell/avoid

    Uses CVXPY if available, else returns a simple linear projection.
    """
    n = len(prices)
    p = np.clip(prices, 1e-6, 1.0 - 1e-6)

    if not HAS_CVXPY or len(polytope.valid_scenarios) == 0:
        # Fallback: normalise prices to sum-to-1 within polytope
        return p / p.sum()

    q = cp.Variable(n, nonneg=True)

    # KL(q||p) = sum q_i*log(q_i/p_i)  — expressed via cp.kl_div
    # cp.kl_div(a, b) = a*log(a/b) - a + b, so sum kl_div(q, p) = KL(q||p) + const
    kl = cp.sum(cp.kl_div(q, p))

    # Polytope membership: q must be a convex combination of valid scenarios
    S = len(polytope.valid_scenarios)
    lam = cp.Variable(S, nonneg=True)
    constraints = [
        cp.sum(lam) == 1,
        polytope.valid_scenarios.T @ lam == q,
        cp.sum(q) == 1,
    ]

    prob = cp.Problem(cp.Minimize(kl), constraints)
    try:
        prob.solve(solver=cp.ECOS, verbose=False)
        if prob.status in ('optimal', 'optimal_inaccurate') and q.value is not None:
            return np.clip(q.value, 1e-6, 1.0)
    except Exception:
        pass

    return p / p.sum()


def frank_wolfe_optimal_bets(prices: np.ndarray, polytope: MarginalPolytope,
                              capital: float, max_iter: int = 200,
                              tol: float = 1e-6) -> Tuple[np.ndarray, float]:
    """
    Frank-Wolfe algorithm to find the optimal bet vector that maximises
    guaranteed profit across all valid outcome scenarios.

    Problem (LP after linearisation):
        maximise   g  -  sum_i(p_i * y_i)
        subject to:
          for each valid scenario s:  sum_{i: s_i=1} y_i  >=  g
          y_i >= 0
          sum_i(p_i * y_i)  =  capital

    where y_i = shares bought of market i  (cost = p_i * y_i per share).

    Frank-Wolfe iterates:
      1. Compute gradient of smooth relaxation
      2. Solve linear oracle (min-cost scenario)
      3. Line-search step toward oracle solution

    Falls back to direct CVXPY LP if FW doesn't converge.

    Returns (y_optimal, guaranteed_profit).
    """
    n = len(prices)
    p = np.clip(prices, 1e-6, 1.0)
    scenarios = polytope.valid_scenarios  # shape (S, N)
    S = len(scenarios)

    # ── Direct LP via CVXPY (primary path) ───────────────────────────────────
    if HAS_CVXPY:
        y = cp.Variable(n, nonneg=True)
        g = cp.Variable()

        budget = cp.sum(cp.multiply(p, y)) == capital

        # Guaranteed payout >= g for every valid scenario
        payout_constraints = [
            scenarios[s] @ y >= g for s in range(S)
        ]

        prob = cp.Problem(
            cp.Maximize(g - capital),
            [budget] + payout_constraints
        )
        try:
            prob.solve(solver=cp.ECOS, verbose=False)
            if prob.status in ('optimal', 'optimal_inaccurate'):
                profit = float(g.value) - capital if g.value is not None else 0.0
                y_val = y.value if y.value is not None else np.zeros(n)
                return np.maximum(y_val, 0), max(profit, 0.0)
        except Exception:
            pass

    # ── Frank-Wolfe fallback ──────────────────────────────────────────────────
    # Initialise: proportional to prices (current heuristic)
    y = np.array(p / p.sum() * capital / p, dtype=float)  # y_i = capital/n / p_i approx
    y = np.maximum(y, 0)

    def guaranteed_payout(y_vec):
        """Worst-case payout across all valid scenarios."""
        payouts = scenarios @ y_vec           # shape (S,)
        return float(payouts.min()) if len(payouts) else 0.0

    def gradient_guaranteed_payout(y_vec):
        """Subgradient: indicator of the worst-case scenario."""
        payouts = scenarios @ y_vec
        worst = int(np.argmin(payouts))
        return scenarios[worst]               # shape (N,)

    for t in range(max_iter):
        g_val = guaranteed_payout(y)
        grad = gradient_guaranteed_payout(y)

        # Linear oracle: maximise grad @ s subject to p @ s = capital, s >= 0
        # Greedy: put all capital on market with highest grad_i / p_i
        ratio = grad / np.maximum(p, 1e-9)
        best_i = int(np.argmax(ratio))
        s = np.zeros(n)
        s[best_i] = capital / p[best_i]

        # Step size: 2/(t+2) (standard FW decay)
        gamma = 2.0 / (t + 2.0)
        y_new = (1 - gamma) * y + gamma * s

        if np.linalg.norm(y_new - y) < tol:
            y = y_new
            break
        y = y_new

    profit = guaranteed_payout(y) - capital
    return np.maximum(y, 0), max(profit, 0.0)


class ArbitrageMathEngine:
    """
    Implements the mathematical core of arbitrage detection and optimization.

    Pipeline per constraint group:
      1. Build MarginalPolytope from constraint type + market relationships
      2. Bregman-project observed prices onto polytope -> detect mispricing
      3. Frank-Wolfe LP -> compute optimal bet sizes for maximum guaranteed profit
      4. Return ArbitrageOpportunity if net profit clears threshold
    """

    def __init__(self, config: Dict, workspace_root: Path):
        self.config = config.get('arbitrage', {})
        self.workspace_root = Path(workspace_root)
        self.logger = logging.getLogger('ArbitrageMathEngine')

        self.min_profit_threshold = self.config.get('min_profit_threshold', 0.03)
        self.max_position_size    = self.config.get('max_position_size', 1000)
        self.trading_fee          = self.config.get('fees', {}).get('trading_fee', 0.0001)
        self.max_profit_threshold = self.config.get('max_profit_threshold', 0.30)
        # Dynamic: will be rescaled per-trade in L4 (dynamic_capital).
        # L3 uses initial_capital × pct as a reference for opportunity sizing only.
        _pct = config.get('capital_per_trade_pct', 0.10)
        _initial = config.get('initial_capital', 100.0)
        self.capital_per_trade    = max(10.0, min(_initial * _pct, 1000.0))

        self.opportunities: List[ArbitrageOpportunity] = []

    # ── Public entry point ────────────────────────────────────────────────────

    def find_arbitrage_opportunities(self, constraints: List, markets: List
                                     ) -> List[ArbitrageOpportunity]:
        self.logger.debug(f"Searching for arbitrage in {len(constraints)} constraints")
        opportunities = []

        for i, constraint in enumerate(constraints, 1):
            try:
                opp = self._check_constraint_for_arbitrage(constraint, markets)
                if opp:
                    self.logger.info(
                        f"  [{i}/{len(constraints)}] ARBITRAGE  "
                        f"type={constraint.relationship_type}  "
                        f"markets={len(constraint.market_ids)}  "
                        f"profit={opp.net_profit:.4f}  "
                        f"method={opp.metadata.get('method','?')}")
                    opportunities.append(opp)
                elif i <= 5 or i % 20 == 0:
                    self.logger.debug(
                        f"  [{i}/{len(constraints)}] No arb: "
                        f"type={constraint.relationship_type}  "
                        f"markets={len(constraint.market_ids)}")
            except Exception as e:
                self.logger.error(f"Error checking constraint {constraint.constraint_id}: {e}")

        self.opportunities = opportunities
        self.logger.info(f"Found {len(opportunities)} arbitrage opportunities")
        return opportunities

    # ── Constraint dispatch ───────────────────────────────────────────────────

    def _check_constraint_for_arbitrage(self, constraint, markets: List
                                        ) -> Optional[ArbitrageOpportunity]:
        from layer2_constraint_detection.constraint_detector import RelationshipType

        market_dict = {m.market_id: m for m in markets}
        constrained_markets = [market_dict[mid] for mid in constraint.market_ids
                                if mid in market_dict]
        if len(constrained_markets) != len(constraint.market_ids):
            return None

        # Check if this is a negRisk constraint group
        is_neg_risk = bool(constraint.metadata.get('negRiskMarketID'))

        rt = constraint.relationship_type

        if rt == RelationshipType.COMPLEMENTARY:
            return self._arb_via_polytope(constraint, constrained_markets,
                                          'complementary')
        elif rt == RelationshipType.MUTUAL_EXCLUSIVITY:
            # Try fast direct check first (handles buy-all and sell-all)
            direct = self._arb_mutex_direct(constraint, constrained_markets)
            if direct:
                return direct
            # Fallback to full polytope/Bregman/FW analysis
            return self._arb_via_polytope(constraint, constrained_markets,
                                          'mutual_exclusivity')
        elif rt == RelationshipType.LOGICAL_IMPLICATION:
            return self._arb_via_polytope(constraint, constrained_markets,
                                          'logical_implication',
                                          implications=[(0, 1)])
        return None


    # -- Fast direct mutex check (buy-YES-all or buy-NO-all) ------

    def _arb_mutex_direct(self, constraint, markets) -> Optional[ArbitrageOpportunity]:
        """
        Direct mutex arb: if N markets are mutually exclusive (exactly 1 wins),
        then sum of YES prices should = 1.0.

        Buy-side arb: sum(YES asks) less than 1.0 -> buy YES on all, guaranteed payout = 1.0
        Sell-side arb: sum(YES asks) greater than 1.0 -> buy NO on all, guaranteed payout = N-1

        Uses ask prices (entry cost) when WS live data available, falls back to midpoint.
        """
        market_ids = [m.market_id for m in markets]
        n = len(markets)

        # YES ask prices (cost to buy YES on each leg)
        yes_prices = {}
        for m in markets:
            p = m.get_entry_price('Yes')
            if p <= 0:
                p = (m.outcome_prices.get('Yes') or
                     m.outcome_prices.get('yes') or
                     m.outcome_prices.get('true') or
                     next(iter(m.outcome_prices.values()), 0.5))
            yes_prices[m.market_id] = float(p)

        p_vec = np.array([yes_prices[mid] for mid in market_ids])

        # Skip dead markets
        if float(p_vec.min()) < 0.02:
            return None

        # SAFETY: price_sum must be close to 1.0 for a valid complete mutex group
        # If sum << 1.0, outcomes are likely incomplete (missing scenarios)
        raw_sum = float(p_vec.sum())
        if raw_sum < 0.90:
            self.logger.debug(f"  SKIP: price_sum={raw_sum:.3f} < 0.90, likely incomplete outcomes")
            return None

        # SAFETY: verify all markets end within 48h of each other (same event)
        end_dates = [m.end_date for m in markets if m.end_date]
        if len(end_dates) >= 2:
            span_hours = (max(end_dates) - min(end_dates)).total_seconds() / 3600
            if span_hours > 48:
                self.logger.debug(f"  SKIP: end_date span={span_hours:.0f}h > 48h, not same event")
                return None

        price_sum = float(p_vec.sum())
        capital = self.capital_per_trade

        # Polymarket fee: ~2% on proceeds from winning contracts
        fee_rate = self.trading_fee  # From config (default 0.0001 = 1bp)

        # --- Buy-side: sum(YES asks) less than 1.0 ---
        if price_sum < 1.0:
            units = capital / price_sum
            cost = capital
            payout = units * 1.0
            fees = payout * fee_rate
            gross_profit = payout - cost
            net_profit = gross_profit - fees
            profit_pct = net_profit / capital

            if profit_pct >= self.min_profit_threshold and profit_pct <= self.max_profit_threshold:
                bets = {mid: float(capital * yes_prices[mid] / price_sum) for mid in market_ids}
                self.logger.debug(
                    f"  MUTEX BUY-ALL: sum={price_sum:.4f} profit={profit_pct:.4f} n={n}")
                return ArbitrageOpportunity(
                    opportunity_id="arb_buy_" + constraint.constraint_id + "_" + str(datetime.now().timestamp()),
                    constraint_id=constraint.constraint_id,
                    market_ids=market_ids,
                    market_names=[m.market_name for m in markets],
                    current_prices=yes_prices,
                    optimal_bets=bets,
                    expected_profit=gross_profit,
                    expected_profit_pct=gross_profit / capital,
                    max_loss=0.0,
                    worst_case_return=profit_pct,
                    total_capital_required=capital,
                    fees_estimated=fees,
                    net_profit=net_profit,
                    detected_at=datetime.now(),
                    expires_at=None,
                    metadata={
                        'method': 'mutex_buy_all',
                        'price_sum': price_sum,
                        'num_markets': n,
                        'fee_rate': fee_rate,
                        'neg_risk': bool(constraint.metadata.get('negRiskMarketID')),
                    }
                )

        # --- Sell-side: sum(YES asks) greater than 1.0 ---
        # Buy NO on all legs. Use actual NO ask prices from book (not 1-YES).
        elif price_sum > 1.0:
            no_prices = {}
            for m in markets:
                no_p = m.get_entry_price('No')
                if no_p <= 0:
                    # Fallback: derive from YES if no NO book data
                    no_p = 1.0 - yes_prices[m.market_id]
                no_prices[m.market_id] = float(no_p)

            no_vec = np.array([no_prices[mid] for mid in market_ids])
            no_cost_per_unit = float(no_vec.sum())
            if no_cost_per_unit <= 0:
                return None

            # negRisk: flag for capital efficiency tracking
            # In negRisk markets, collateral locked per unit set = $1.00 (vs sum(NO_ask))
            # Profit calculation is the same — determined by book prices
            # Capital efficiency matters for live trading collateral, not paper P&L
            is_neg_risk = bool(constraint.metadata.get('negRiskMarketID'))
            collateral_per_unit = 1.0 if is_neg_risk else no_cost_per_unit
            cap_efficiency = no_cost_per_unit / collateral_per_unit if collateral_per_unit > 0 else 1.0

            units = capital / no_cost_per_unit
            cost = capital
            payout = units * (n - 1)
            fees = payout * fee_rate
            gross_profit = payout - cost
            net_profit = gross_profit - fees
            profit_pct = net_profit / capital

            if profit_pct >= self.min_profit_threshold and profit_pct <= self.max_profit_threshold:
                bets = {mid: float(capital * no_prices[mid] / no_cost_per_unit) for mid in market_ids}
                self.logger.debug(
                    f"  MUTEX SELL-ALL: yes_sum={price_sum:.4f} no_sum={no_cost_per_unit:.4f} "
                    f"profit={profit_pct:.4f} n={n}"
                    f"{f' negRisk: {cap_efficiency:.1f}x cap efficient' if is_neg_risk else ''}")
                return ArbitrageOpportunity(
                    opportunity_id="arb_sell_" + constraint.constraint_id + "_" + str(datetime.now().timestamp()),
                    constraint_id=constraint.constraint_id,
                    market_ids=market_ids,
                    market_names=[m.market_name for m in markets],
                    current_prices=yes_prices,
                    optimal_bets=bets,
                    expected_profit=gross_profit,
                    expected_profit_pct=gross_profit / capital,
                    max_loss=0.0,
                    worst_case_return=profit_pct,
                    total_capital_required=capital,
                    fees_estimated=fees,
                    net_profit=net_profit,
                    detected_at=datetime.now(),
                    expires_at=None,
                    metadata={
                        'method': 'mutex_sell_all',
                        'price_sum': price_sum,
                        'no_price_sum': no_cost_per_unit,
                        'num_markets': n,
                        'fee_rate': fee_rate,
                        'neg_risk': is_neg_risk,
                        'collateral_per_unit': collateral_per_unit,
                        'capital_efficiency': cap_efficiency,
                    }
                )

        return None

    # ── Core: Marginal Polytope + Bregman + Frank-Wolfe ──────────────────────

    def _arb_via_polytope(self, constraint, markets: List,
                           constraint_type: str,
                           implications: List[Tuple[int,int]] = None
                           ) -> Optional[ArbitrageOpportunity]:
        """
        Full pipeline:
          1. Extract YES prices
          2. Build marginal polytope
          3. Bregman-project prices onto polytope
          4. If mispricing >= threshold: Frank-Wolfe optimal bets
          5. Return opportunity
        """
        market_ids = [m.market_id for m in markets]
        prices_raw = {}
        for m in markets:
            p = m.get_entry_price('Yes')
            if p <= 0:
                p = (m.outcome_prices.get('Yes') or
                     m.outcome_prices.get('yes') or
                     m.outcome_prices.get('true') or
                     next(iter(m.outcome_prices.values()), 0.5))
            prices_raw[m.market_id] = float(p)

        p_vec = np.array([prices_raw[mid] for mid in market_ids])
        n = len(p_vec)

        # Guard: skip polytope for large market groups (2^N explodes memory)
        MAX_POLYTOPE_MARKETS = 12
        if n > MAX_POLYTOPE_MARKETS:
            self.logger.debug(f"  Skipping polytope: {n} markets > {MAX_POLYTOPE_MARKETS} max")
            return None

        # SAFETY: For mutex groups, price_sum must be near 1.0
        # If sum << 1.0, the group may be incomplete (missing outcomes)
        price_sum = float(p_vec.sum())
        if constraint_type in ('mutual_exclusivity', 'mutex'):
            if price_sum < 0.90:
                self.logger.debug(
                    f"  SKIP incomplete mutex in polytope: sum={price_sum:.3f} < 0.90, "
                    f"missing ~{(1.0-price_sum)*100:.1f}% of outcomes")
                return None

        # ── 1. Build marginal polytope ────────────────────────────────────────
        polytope = MarginalPolytope(market_ids, constraint_type,
                                    implications=implications)

        if len(polytope.valid_scenarios) == 0:
            return None

        # ── 2. Bregman projection -> detect mispricing ────────────────────────
        q_vec = bregman_project_kl(p_vec, polytope)
        mispricing = float(np.linalg.norm(p_vec - q_vec, ord=1))  # L1 distance

        # Quick sum check (catches most cases fast before running LP)
        price_sum = float(p_vec.sum())
        effective_fee = self.trading_fee * n  # 1bp per leg (near-zero impact)

        # Sanity guards: skip dead/expired markets and bad constraints
        # Any individual price below 2 cents = no real liquidity
        if float(p_vec.min()) < 0.02:
            return None
        # For 2-market mutex/complementary: must sum >= 0.80
        # Lower means these are just 2 candidates in a larger field, not a binary pair
        if n == 2 and price_sum < 0.80:
            return None
        # Multi-market: sum must be meaningful (>= 0.30) and not absurd (< 1.40)
        if price_sum < 0.30 or price_sum > 1.40:
            return None

        if price_sum >= (1.0 - effective_fee) and mispricing < 0.01:
            return None

        # ── 3. Frank-Wolfe optimal bets ───────────────────────────────────────
        capital = self.capital_per_trade
        y_opt, gross_profit = frank_wolfe_optimal_bets(p_vec, polytope, capital)

        fees = capital * effective_fee
        net_profit = gross_profit - fees

        if net_profit / capital < self.min_profit_threshold:
            self.logger.debug(
                f"  Mispricing {mispricing:.4f} but net profit "
                f"{net_profit/capital:.4f} < threshold {self.min_profit_threshold}")
            return None
        # Cap max profit at 15% - genuine liquid arb is 1-8%, above 15% = bad constraint
        max_profit_threshold = self.max_profit_threshold  # From config (default 0.30)
        if net_profit / capital > max_profit_threshold:
            self.logger.debug(
                f"  Profit {net_profit/capital:.4f} > max {max_profit_threshold} - likely bad constraint")
            return None

        # ── 4. Build result ───────────────────────────────────────────────────
        # y_opt is shares; convert to dollar bets: bet_i = y_i * p_i
        bets = {mid: float(y_opt[i] * p_vec[i])
                for i, mid in enumerate(market_ids)}

        return ArbitrageOpportunity(
            opportunity_id=f"arb_{constraint.constraint_id}_{datetime.now().timestamp()}",
            constraint_id=constraint.constraint_id,
            market_ids=market_ids,
            market_names=[m.market_name for m in markets],
            current_prices=prices_raw,
            optimal_bets=bets,
            expected_profit=gross_profit,
            expected_profit_pct=gross_profit / capital,
            max_loss=0.0,
            worst_case_return=net_profit / capital,
            total_capital_required=capital,
            fees_estimated=fees,
            net_profit=net_profit,
            detected_at=datetime.now(),
            expires_at=None,
            metadata={
                'method': 'bregman_frank_wolfe',
                'constraint_type': str(constraint_type),
                'num_markets': n,
                'price_sum': price_sum,
                'mispricing_l1': mispricing,
                'valid_scenarios': len(polytope.valid_scenarios),
                'bregman_projection': q_vec.tolist(),
                'neg_risk': bool(constraint.metadata.get('negRiskMarketID')),
            }
        )

    # ── Persistence ───────────────────────────────────────────────────────────

    def save_opportunities(self, output_path: Path):
        output_path.parent.mkdir(parents=True, exist_ok=True)
        data = {
            'timestamp': datetime.now().isoformat(),
            'opportunity_count': len(self.opportunities),
            'total_potential_profit': sum(o.net_profit for o in self.opportunities),
            'opportunities': [o.to_dict() for o in self.opportunities]
        }
        with open(output_path, 'w') as f:
            json.dump(data, f, indent=2)
        self.logger.info(f"Saved {len(self.opportunities)} opportunities to {output_path}")


if __name__ == '__main__':
    logging.basicConfig(level=logging.INFO)
    print("Testing ArbitrageMathEngine with advanced maths...")

    # Synthetic test: 3 mutex markets, prices sum to 0.88 (arb exists)
    from types import SimpleNamespace
    from layer2_constraint_detection.constraint_detector import RelationshipType

    prices_test = [0.30, 0.28, 0.30]   # sum=0.88, should find arb
    market_ids_t = ['m1', 'm2', 'm3']

    polytope_t = MarginalPolytope(market_ids_t, 'mutual_exclusivity')
    print(f"Valid scenarios: {len(polytope_t.valid_scenarios)}")
    print(f"Prices in polytope: {polytope_t.contains(np.array(prices_test))}")

    p_vec_t = np.array(prices_test)
    q_vec_t = bregman_project_kl(p_vec_t, polytope_t)
    print(f"Observed prices:    {p_vec_t}")
    print(f"Bregman projection: {q_vec_t.round(4)}")
    print(f"Mispricing (L1):    {np.linalg.norm(p_vec_t - q_vec_t, ord=1):.4f}")

    y_opt_t, profit_t = frank_wolfe_optimal_bets(p_vec_t, polytope_t, 100.0)
    bets_t = y_opt_t * p_vec_t
    print(f"Optimal bets ($):   {bets_t.round(2)}")
    print(f"Gross profit ($):   {profit_t:.4f}")
    print(f"Profit %:           {profit_t:.2%}")
