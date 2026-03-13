"""Resolution Date Validator - Uses Anthropic API to extract true resolution dates from market rules.

Polymarket's endDate field often reflects the *earliest* resolution date,
not the *latest*. Market rules frequently contain fallback/extension clauses
(e.g. "if no result by X, resolves to Y by Z").

This module:
1. Fetches market description (rules) from Polymarket API
2. Sends to Claude to extract the LATEST possible resolution date
3. Caches results to disk (rules don't change)
"""
import json, logging, os, time, hashlib
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional, Dict

import requests

log = logging.getLogger('resolution_validator')

POLYMARKET_MARKET_URL = 'https://gamma-api.polymarket.com/markets/{market_id}'
ANTHROPIC_API_URL = 'https://api.anthropic.com/v1/messages'
CACHE_DIR = Path('/home/andydoc/prediction-trader/data/resolution_cache')

EXTRACTION_PROMPT = """You are analyzing prediction market rules to extract the LATEST possible resolution date and check for unrepresented outcomes.

Market question: {question}

Market rules/description:
{description}

API-provided end date: {api_end_date}

Instructions:
1. Read the rules carefully for ALL resolution scenarios
2. Identify the LATEST date by which this market MUST resolve (including extensions, fallbacks, "if results not known by X" clauses)
3. If no explicit latest date exists, use the API end date
4. Check if the rules mention ANY outcome that could occur but is NOT covered by any named market in the group. Examples:
   - "Resolves to Other if no primary takes place" → has_unrepresented_outcome: true (explicit "Other" catch-all)
   - "Including any potential run-off" + only 2 candidates listed → has_unrepresented_outcome: true (runoff could produce different winner)
   - Netflix rankings where unlisted movies could place → has_unrepresented_outcome: true

CRITICAL — DO NOT flag these as unrepresented outcomes:
   - Sports match timing rules ("after 80 minutes", "after 90 minutes", "at full time") — these define WHEN the result is read, not extra outcomes. Win/Draw/Lose is always exhaustive at any timepoint.
   - Cancellation or void clauses ("if canceled, resolves to No") — this is an event-level risk, not a missing match outcome. A canceled match doesn't create a fourth result between Win/Draw/Lose.
   - Postponement clauses ("remains open until played") — not an outcome, just a delay.
   - Standard Yes/No market structure where "Otherwise resolves to No" — "No" already covers all non-winning outcomes.

5. Return ONLY a JSON object, no other text:

{{"latest_resolution_date": "YYYY-MM-DD", "confidence": "high|medium|low", "has_unrepresented_outcome": true|false, "unrepresented_outcome_reason": "explanation if true, empty string if false", "reasoning": "one sentence about the date"}}

Examples:
- "If no election by Dec 31, resolves to X" → latest is Dec 31
- "Resolves when official results released, no later than June 30" → latest is June 30
- "If no primary takes place, resolves to Other" → has_unrepresented_outcome: true
- "Result after 80 minutes of play, canceled → No" → has_unrepresented_outcome: false (timing rule + cancellation, NOT a missing outcome)
- "Top Netflix movie this week" with only 5 movies listed → has_unrepresented_outcome: true (unlisted movie could win)
"""


def _get_cache_path(group_id: str) -> Path:
    """Cache path for a market group."""
    safe = hashlib.md5(group_id.encode()).hexdigest()[:12]
    return CACHE_DIR / f'{safe}.json'


def _load_cache(group_id: str) -> Optional[Dict]:
    """Load cached validation result. Cache expires after 7 days."""
    path = _get_cache_path(group_id)
    if not path.exists():
        return None
    try:
        data = json.loads(path.read_text())
        # Expire after 7 days
        cached_at = data.get('cached_at', 0)
        if time.time() - cached_at > 7 * 86400:
            return None
        return data
    except Exception:
        return None


def _save_cache(group_id: str, result: Dict):
    """Save validation result to disk cache."""
    CACHE_DIR.mkdir(parents=True, exist_ok=True)
    result['cached_at'] = time.time()
    _get_cache_path(group_id).write_text(json.dumps(result, indent=2))


def fetch_market_description(market_id: int) -> Optional[Dict]:
    """Fetch market details including description/rules from Polymarket API."""
    try:
        resp = requests.get(
            POLYMARKET_MARKET_URL.format(market_id=market_id),
            timeout=10
        )
        if resp.ok:
            data = resp.json()
            return {
                'description': data.get('description', ''),
                'question': data.get('question', ''),
                'end_date': data.get('endDate', ''),
                'neg_risk_market_id': data.get('negRiskMarketID', ''),
            }
        log.warning(f'Polymarket API returned {resp.status_code} for market {market_id}')
    except Exception as e:
        log.warning(f'Failed to fetch market {market_id}: {e}')
    return None


def call_anthropic_api(question: str, description: str, api_end_date: str,
                       api_key: str, ai_config: dict = None) -> Optional[Dict]:
    """Call Anthropic API to extract latest resolution date from rules text.
    
    Uses model/tokens from ai_config if provided, else falls back to defaults.
    Prompt loaded from config/prompts.yaml if available, else uses built-in EXTRACTION_PROMPT.
    """
    if not api_key:
        log.warning('No Anthropic API key configured')
        return None

    # Load prompt from config or use built-in
    prompt_template = EXTRACTION_PROMPT
    try:
        import yaml as _yaml
        _prompts_path = Path('/home/andydoc/prediction-trader/config/prompts.yaml')
        if _prompts_path.exists():
            _pdata = _yaml.safe_load(_prompts_path.read_text())
            if _pdata and 'resolution_validation' in _pdata:
                prompt_template = _pdata['resolution_validation']
    except Exception:
        pass  # Fall back to built-in

    prompt = prompt_template.format(
        question=question,
        description=description,
        api_end_date=api_end_date
    )

    # Model and tokens from config or defaults
    model = 'claude-sonnet-4-20250514'
    max_tokens = 256
    api_url = ANTHROPIC_API_URL
    api_version = '2023-06-01'
    if ai_config:
        model = ai_config.get('models', {}).get('resolution_validation', model)
        max_tokens = ai_config.get('max_tokens', {}).get('resolution_validation', max_tokens)
        api_url = ai_config.get('api_url', api_url)
        api_version = ai_config.get('api_version', api_version)

    try:
        resp = requests.post(
            api_url,
            headers={
                'Content-Type': 'application/json',
                'x-api-key': api_key,
                'anthropic-version': api_version,
            },
            json={
                'model': model,
                'max_tokens': max_tokens,
                'messages': [{'role': 'user', 'content': prompt}],
            },
            timeout=30
        )

        if not resp.ok:
            log.warning(f'Anthropic API returned {resp.status_code}: {resp.text[:200]}')
            return None

        data = resp.json()
        text = ''
        for block in data.get('content', []):
            if block.get('type') == 'text':
                text += block['text']

        # Parse JSON from response
        text = text.strip()
        # Handle markdown code blocks
        if text.startswith('```'):
            text = text.split('\n', 1)[1] if '\n' in text else text[3:]
            if text.endswith('```'):
                text = text[:-3]
            text = text.strip()

        result = json.loads(text)
        log.info(f'AI validation: date={result.get("latest_resolution_date")} '
                 f'confidence={result.get("confidence")} '
                 f'unrepresented_outcome={result.get("has_unrepresented_outcome", False)} '
                 f'reason={result.get("reasoning","")[:80]}')
        return result

    except json.JSONDecodeError as e:
        log.warning(f'Failed to parse Anthropic response as JSON: {e}, text={text[:200]}')
        return None
    except Exception as e:
        log.error(f'Anthropic API call failed: {e}')
        return None


def get_full_validation(
    market_ids: list,
    market_lookup: dict,
    api_key: str,
    group_id: str = None
) -> Optional[Dict]:
    """Get full AI validation result including date and unrepresented outcome flag.

    Returns dict with keys: latest_resolution_date, confidence, has_unrepresented_outcome,
    unrepresented_outcome_reason, reasoning.  Or None if validation fails.
    """
    if not market_ids:
        return None

    first_mid = str(market_ids[0])
    if not group_id:
        md = market_lookup.get(first_mid)
        if md and hasattr(md, 'metadata') and md.metadata.get('negRiskMarketID'):
            group_id = md.metadata.get('negRiskMarketID', first_mid)
        else:
            group_id = first_mid

    cached = _load_cache(group_id)
    if cached and 'latest_resolution_date' in cached:
        return cached

    first_market_id = int(market_ids[0])
    details = fetch_market_description(first_market_id)
    if not details or not details.get('description'):
        return None

    result = call_anthropic_api(
        question=details['question'],
        description=details['description'],
        api_end_date=details['end_date'],
        api_key=api_key
    )

    if not result or 'latest_resolution_date' not in result:
        return None

    _save_cache(group_id, result)
    return result


def get_validated_resolution_date(
    market_ids: list,
    market_lookup: dict,
    api_key: str,
    group_id: str = None
) -> Optional[datetime]:
    """Get AI-validated latest resolution date for a market group.

    Args:
        market_ids: List of market IDs in the group
        market_lookup: Dict of market_id -> MarketData objects
        api_key: Anthropic API key
        group_id: Cache key (constraint_id or negRiskMarketID)

    Returns:
        datetime of latest possible resolution, or None if validation fails
        (caller should fall back to API end_date)
    """
    if not market_ids:
        return None

    # Use first market's negRiskMarketID or constraint_id as group key
    if not group_id:
        first_mid = str(market_ids[0])
        md = market_lookup.get(first_mid)
        if md and hasattr(md, 'metadata'):
            group_id = md.metadata.get('negRiskMarketID', first_mid)
        else:
            group_id = first_mid

    # Check cache first
    cached = _load_cache(group_id)
    if cached and 'latest_resolution_date' in cached:
        try:
            dt = datetime.strptime(cached['latest_resolution_date'], '%Y-%m-%d')
            return dt.replace(hour=23, minute=59, second=59, tzinfo=timezone.utc)
        except ValueError:
            pass  # Cache corrupted, re-validate

    # Fetch description from first market
    first_market_id = int(market_ids[0])
    details = fetch_market_description(first_market_id)
    if not details or not details.get('description'):
        log.debug(f'No description available for market {first_market_id}')
        return None

    # Call Anthropic API
    result = call_anthropic_api(
        question=details['question'],
        description=details['description'],
        api_end_date=details['end_date'],
        api_key=api_key
    )

    if not result or 'latest_resolution_date' not in result:
        return None

    # Cache the result
    _save_cache(group_id, result)

    # Parse and return
    try:
        dt = datetime.strptime(result['latest_resolution_date'], '%Y-%m-%d')
        return dt.replace(hour=23, minute=59, second=59, tzinfo=timezone.utc)
    except ValueError as e:
        log.warning(f'Could not parse AI date "{result["latest_resolution_date"]}": {e}')
        return None
