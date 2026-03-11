"""Postponement Detector — AI-powered web search for rescheduled events.

When a position's expected resolution date passes without the market resolving,
this module searches the internet to find:
  1. Whether the event was postponed/cancelled/rescheduled
  2. The new scheduled date (or a window)
  3. The season end date as fallback

Uses Anthropic API with web_search tool. Config-driven prompts and models.
Two-attempt strategy: if first call finds postponement but no date, a retry
with context injection uses different search strategies.

Results are cached with configurable TTL (default 24h).
"""
import json, logging, re, time, hashlib
from datetime import datetime, timezone, timedelta
from pathlib import Path
from typing import Optional, Dict

import requests
import yaml

log = logging.getLogger('postponement_detector')

WORKSPACE = Path('/home/andydoc/prediction-trader')
CACHE_DIR = WORKSPACE / 'data' / 'postponement_cache'
PROMPTS_PATH = WORKSPACE / 'config' / 'prompts.yaml'
SECRETS_PATH = WORKSPACE / 'config' / 'secrets.yaml'

# --- Prompt and config loading ---

_prompts_cache = {'data': None, 'loaded_at': 0}

def _load_prompts() -> dict:
    """Load prompts from config/prompts.yaml (cached for 1 hour)."""
    if _prompts_cache['data'] and (time.time() - _prompts_cache['loaded_at']) < 3600:
        return _prompts_cache['data']
    try:
        data = yaml.safe_load(PROMPTS_PATH.read_text())
        _prompts_cache.update(data=data, loaded_at=time.time())
        return data
    except Exception as e:
        log.warning(f'Failed to load prompts.yaml: {e}')
        return {}


def _get_api_key() -> str:
    """Load API key from secrets.yaml."""
    try:
        secrets = yaml.safe_load(SECRETS_PATH.read_text())
        return secrets.get('resolution_validation', {}).get('anthropic_api_key', '')
    except Exception:
        return ''


# --- Cache ---

def _cache_key(position_id: str) -> str:
    return hashlib.md5(position_id.encode()).hexdigest()[:16]


def _load_from_cache(position_id: str, ttl_hours: float = 24) -> Optional[Dict]:
    path = CACHE_DIR / f'{_cache_key(position_id)}.json'
    if not path.exists():
        return None
    try:
        data = json.loads(path.read_text())
        if time.time() - data.get('cached_at', 0) > ttl_hours * 3600:
            return None
        return data
    except Exception:
        return None


def _save_to_cache(position_id: str, result: Dict):
    CACHE_DIR.mkdir(parents=True, exist_ok=True)
    result['cached_at'] = time.time()
    result['position_id'] = position_id
    (CACHE_DIR / f'{_cache_key(position_id)}.json').write_text(
        json.dumps(result, indent=2))


def _extract_json(text: str) -> Optional[Dict]:
    """Extract JSON from model response that may have text/markdown around it."""
    text = text.strip()
    try:
        return json.loads(text)
    except json.JSONDecodeError:
        pass
    # Try markdown code block
    m = re.search(r'```(?:json)?\s*\n?(.*?)\n?```', text, re.DOTALL)
    if m:
        try:
            return json.loads(m.group(1).strip())
        except json.JSONDecodeError:
            pass
    # Find last JSON object in text (model often puts explanation before JSON)
    matches = list(re.finditer(r'\{[^{}]*(?:\{[^{}]*\}[^{}]*)*\}', text, re.DOTALL))
    for m in reversed(matches):
        try:
            return json.loads(m.group(0))
        except json.JSONDecodeError:
            continue
    return None


# --- Rate limiting ---
_last_api_call = 0.0


def _call_anthropic(prompt: str, ai_config: dict) -> Optional[Dict]:
    """Call Anthropic API with web_search tool. Returns parsed JSON or None."""
    global _last_api_call

    api_key = _get_api_key()
    if not api_key:
        log.warning('No Anthropic API key — cannot check postponement')
        return None

    # Rate limit
    min_gap = ai_config.get('postponement', {}).get('rate_limit_seconds', 60)
    elapsed = time.time() - _last_api_call
    if elapsed < min_gap:
        wait = min_gap - elapsed
        log.debug(f'Rate limit: waiting {wait:.0f}s before API call')
        time.sleep(wait)

    model = ai_config.get('models', {}).get(
        'postponement_detection', 'claude-sonnet-4-20250514')
    max_tokens = ai_config.get('max_tokens', {}).get(
        'postponement_detection', 1024)
    api_url = ai_config.get('api_url',
        'https://api.anthropic.com/v1/messages')
    api_version = ai_config.get('api_version', '2023-06-01')

    try:
        _last_api_call = time.time()
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
                'tools': [{'type': 'web_search_20250305', 'name': 'web_search'}],
                'messages': [{'role': 'user', 'content': prompt}],
            },
            timeout=90
        )

        if not resp.ok:
            log.warning(f'Anthropic API {resp.status_code}: {resp.text[:200]}')
            return None

        data = resp.json()
        text_parts = []
        search_count = 0
        for block in data.get('content', []):
            if block.get('type') == 'text':
                text_parts.append(block['text'])
            elif block.get('type') == 'web_search_tool_result':
                search_count += 1

        full_text = '\n'.join(text_parts)
        log.info(f'Postponement API: {search_count} web searches, '
                 f'{len(full_text)} chars response')

        result = _extract_json(full_text)
        if result:
            result['_search_count'] = search_count
        return result

    except Exception as e:
        log.error(f'Postponement API call failed: {e}')
        return None


# --- Apply date buffer: +24h rounded up to next midnight UTC ---

def apply_date_buffer(date_str: str, buffer_hours: int = 24) -> str:
    """Add buffer_hours to a date string and round up to next midnight UTC.
    Returns YYYY-MM-DD string."""
    try:
        dt = datetime.strptime(date_str, '%Y-%m-%d').replace(tzinfo=timezone.utc)
        dt += timedelta(hours=buffer_hours)
        # Round up to next midnight (if not already midnight)
        if dt.hour > 0 or dt.minute > 0 or dt.second > 0:
            dt = (dt + timedelta(days=1)).replace(hour=0, minute=0, second=0)
        return dt.strftime('%Y-%m-%d')
    except Exception:
        return date_str


# === Main public API ===

def check_postponement(
    position_id: str,
    market_names: list,
    original_date: str,
    ai_config: dict,
) -> Optional[Dict]:
    """Check if a position's event has been postponed/rescheduled.

    Args:
        position_id: Unique position identifier (for caching)
        market_names: List of market name strings from the position
        original_date: Expected resolution date (YYYY-MM-DD)
        ai_config: The ai: section from config.yaml

    Returns:
        Dict with keys: status, new_date, date_confidence, window_end,
        season_end, reason, sources, effective_resolution_date.
        Or None if check fails/disabled.
    """
    pp_cfg = ai_config.get('postponement', {})
    if not pp_cfg.get('enabled', True):
        return None

    # Check cache first
    cache_ttl = pp_cfg.get('cache_ttl_hours', 24)
    cached = _load_from_cache(position_id, cache_ttl)
    if cached:
        log.debug(f'Postponement cache hit for {position_id[:30]}')
        return cached

    now = datetime.now(timezone.utc)
    today_str = now.strftime('%Y-%m-%d')
    try:
        orig_dt = datetime.strptime(original_date, '%Y-%m-%d').replace(
            tzinfo=timezone.utc)
        days_overdue = max(0, (now - orig_dt).days)
    except Exception:
        days_overdue = 0

    # Load prompt template
    prompts = _load_prompts()
    prompt_template = prompts.get('postponement_detection', '')
    if not prompt_template:
        log.warning('No postponement_detection prompt in prompts.yaml')
        return None

    market_names_str = json.dumps(market_names)

    # === Attempt 1: General search ===
    prompt = prompt_template.format(
        market_names=market_names_str,
        original_date=original_date,
        today=today_str,
        days_overdue=days_overdue,
    )
    log.info(f'Postponement check: {market_names[0][:50]}... '
             f'(overdue {days_overdue}d)')

    result = _call_anthropic(prompt, ai_config)

    max_attempts = pp_cfg.get('max_attempts', 2)
    buffer_hours = pp_cfg.get('date_buffer_hours', 24)

    # === Attempt 2: Targeted retry if postponed but no date ===
    if (result
            and result.get('status') == 'postponed'
            and not result.get('new_date')
            and max_attempts >= 2):
        log.info(f'  Attempt 1 found postponement but no date — retrying '
                 f'with context injection')
        retry_template = prompts.get('postponement_retry', '')
        if retry_template:
            retry_prompt = retry_template.format(
                prev_status=result.get('status', ''),
                prev_reason=result.get('reason', ''),
                prev_sources=json.dumps(result.get('sources', [])),
                prev_queries=json.dumps(
                    result.get('search_queries_used', [])),
                prev_season_end=result.get('season_end', ''),
                market_names=market_names_str,
                original_date=original_date,
                today=today_str,
            )
            result2 = _call_anthropic(retry_prompt, ai_config)
            if result2 and result2.get('new_date'):
                log.info(f'  Attempt 2 found date: {result2["new_date"]}')
                result = result2
            elif result2:
                # Merge: keep attempt 2 sources/queries, inherit season_end
                result.setdefault('season_end', result2.get('season_end'))
                result['sources'] = list(set(
                    result.get('sources', []) +
                    result2.get('sources', [])))

    if not result:
        log.warning(f'Postponement check failed for {position_id[:30]}')
        return None

    # === Compute effective_resolution_date ===
    raw_date = result.get('new_date')
    confidence = result.get('date_confidence', 'unknown')
    season_end = result.get('season_end')
    fallback = pp_cfg.get('fallback_to_season_end', True)

    if raw_date:
        effective = apply_date_buffer(raw_date, buffer_hours)
    elif fallback and season_end:
        effective = apply_date_buffer(season_end, buffer_hours)
        result['date_confidence'] = 'season_end'
        result['new_date'] = season_end
        log.info(f'  No date found — falling back to season end: '
                 f'{season_end} (+{buffer_hours}h = {effective})')
    else:
        effective = None

    result['effective_resolution_date'] = effective
    result['checked_at'] = now.isoformat()
    result['original_date'] = original_date
    result['days_overdue'] = days_overdue

    # Cache result
    _save_to_cache(position_id, result)
    log.info(f'  Result: status={result.get("status")} '
             f'date={raw_date} confidence={confidence} '
             f'effective={effective}')

    return result
