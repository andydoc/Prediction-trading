"""
Layer 2: Constraint Detection (v2 - API-based grouping)
Uses Polymarket's negRiskMarketID to find genuine mutex groups.
NO text-based heuristics - only API-confirmed relationships.
"""

import logging
from dataclasses import dataclass
from datetime import datetime
from enum import Enum
from typing import List, Dict, Set, Tuple, Optional
from pathlib import Path
import json


class RelationshipType(Enum):
    MUTUAL_EXCLUSIVITY = "mutex"
    COMPLEMENTARY = "complement"


@dataclass
class MarketConstraint:
    constraint_id: str
    market_ids: List[str]
    market_names: List[str]
    relationship_type: RelationshipType
    confidence: float
    formula: str
    detected_at: datetime
    metadata: Dict

    def to_dict(self) -> Dict:
        return {
            'constraint_id': self.constraint_id,
            'market_ids': self.market_ids,
            'market_names': self.market_names,
            'relationship_type': self.relationship_type.value,
            'confidence': self.confidence,
            'formula': self.formula,
            'detected_at': self.detected_at.isoformat(),
            'metadata': self.metadata
        }


class ConstraintDetector:
    """Detects logical constraints using Polymarket API grouping fields."""

    def __init__(self, config: Dict, workspace_root: Path):
        self.config = config.get('constraint_detection', {})
        self.workspace_root = Path(workspace_root)
        self.logger = logging.getLogger('ConstraintDetector')
        self.constraints: List[MarketConstraint] = []
        # Price sum bounds for valid mutex group
        self.min_price_sum = self.config.get('min_price_sum', 0.85)
        self.max_price_sum = self.config.get('max_price_sum', 1.15)

    def detect_constraints(self, markets: List) -> List[MarketConstraint]:
        self.logger.info(f"Detecting constraints across {len(markets)} markets")
        constraints = []

        # Primary: group by negRiskMarketID
        constraints.extend(self._detect_neg_risk_mutex(markets))

        self.constraints = constraints
        self.logger.info(f"Detected {len(constraints)} constraints "
                         f"({sum(1 for c in constraints if c.relationship_type == RelationshipType.MUTUAL_EXCLUSIVITY)} mutex)")
        return constraints

    def _detect_neg_risk_mutex(self, markets: List) -> List[MarketConstraint]:
        """Group markets by negRiskMarketID — the authoritative mutex signal."""
        constraints = []
        groups: Dict[str, List] = {}

        for m in markets:
            meta = m.metadata if isinstance(m.metadata, dict) else {}
            neg_risk = meta.get('negRisk', False)
            nrm_id = meta.get('negRiskMarketID', '')

            if neg_risk and nrm_id:
                if nrm_id not in groups:
                    groups[nrm_id] = []
                groups[nrm_id].append(m)

        self.logger.debug(f"Found {len(groups)} negRisk groups, "
                          f"{sum(1 for g in groups.values() if len(g) >= 2)} with 2+ markets")

        for nrm_id, group_markets in groups.items():
            if len(group_markets) < 2:
                continue

            # Get YES prices
            prices = {}
            for m in group_markets:
                p = (m.outcome_prices.get('Yes') or
                     m.outcome_prices.get('yes') or
                     m.outcome_prices.get('true') or
                     next(iter(m.outcome_prices.values()), 0.5))
                prices[m.market_id] = float(p)

            price_sum = sum(prices.values())

            # Completeness check: sum should be near 1.0
            if price_sum < self.min_price_sum:
                self.logger.debug(
                    f"  SKIP incomplete group {nrm_id[:16]}...: "
                    f"{len(group_markets)} mkts, sum={price_sum:.3f} < {self.min_price_sum}")
                continue

            if price_sum > self.max_price_sum:
                self.logger.debug(
                    f"  SKIP over-priced group {nrm_id[:16]}...: "
                    f"{len(group_markets)} mkts, sum={price_sum:.3f} > {self.max_price_sum}")
                continue

            market_ids = [m.market_id for m in group_markets]
            market_names = [m.market_name for m in group_markets]
            formula = " + ".join(f"P({mid})" for mid in market_ids) + " = 1.0"
            group_titles = [
                (m.metadata.get('groupItemTitle', '') if isinstance(m.metadata, dict) else '')
                for m in group_markets
            ]

            constraint = MarketConstraint(
                constraint_id=f"mutex_{nrm_id[:32]}",
                market_ids=market_ids,
                market_names=market_names,
                relationship_type=RelationshipType.MUTUAL_EXCLUSIVITY,
                confidence=0.98,  # API-confirmed grouping
                formula=formula,
                detected_at=datetime.now(),
                metadata={
                    'negRiskMarketID': nrm_id,
                    'num_outcomes': len(group_markets),
                    'price_sum': price_sum,
                    'group_titles': group_titles,
                    'detection_method': 'negRiskMarketID',
                }
            )
            constraints.append(constraint)

        return constraints

    def save_constraints(self, output_path: Path):
        output_path.parent.mkdir(parents=True, exist_ok=True)
        data = {
            'timestamp': datetime.now().isoformat(),
            'constraint_count': len(self.constraints),
            'constraints': [c.to_dict() for c in self.constraints]
        }
        with open(output_path, 'w') as f:
            json.dump(data, f, indent=2)
        self.logger.info(f"Saved {len(self.constraints)} constraints to {output_path}")

    def load_constraints(self, input_path: Path) -> List[MarketConstraint]:
        with open(input_path, 'r') as f:
            data = json.load(f)
        constraints = []
        for c_dict in data['constraints']:
            c_dict['detected_at'] = datetime.fromisoformat(c_dict['detected_at'])
            c_dict['relationship_type'] = RelationshipType(c_dict['relationship_type'])
            constraints.append(MarketConstraint(**c_dict))
        self.logger.info(f"Loaded {len(constraints)} constraints from {input_path}")
        return constraints
