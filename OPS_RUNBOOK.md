# Operations Runbook

**System**: Prediction Market Arbitrage Trading System
**Binary**: `prediction-trader` (Rust, single binary)
**Version**: v0.14.7

---

## 1. VPS Details

| Item | Value |
|------|-------|
| Provider | ZAP-Hosting (Germany) |
| IP | `193.23.127.99` |
| SSH | `ssh vps-ubuntu` (ubuntu user) or `ssh vps` (root) |
| Specs | 4 vCPU, 8 GB RAM, 25 GB NVMe, Ubuntu 24.04 |
| Workspace | `/home/ubuntu/prediction-trader` |
| Binary | `/home/ubuntu/prediction-trader/target/release/prediction-trader` |

**SSH quick commands:**
```bash
ssh vps-ubuntu                                  # Shell (ubuntu user)
ssh vps-ubuntu 'systemctl status prediction-trader'
ssh vps-ubuntu 'tail -50 /home/ubuntu/prediction-trader/logs/supervisor-*.log'
```

**ZAP-Hosting login reminder**: Set a calendar reminder every 3 months to log in to the ZAP dashboard and verify account/billing status.

---

## 2. Scripts

All scripts are in `scripts/`. Set `TRADER_WORKSPACE` env var or they default to `/home/andydoc/prediction-trader` (dev) or `/home/ubuntu/prediction-trader` (VPS).

### start.sh — Start the system
```bash
bash start.sh                                    # Shadow mode (default)
bash start.sh --mode live                        # Live trading
bash start.sh --instance shadow-a                # Multi-instance
bash start.sh --set arbitrage.min_profit_threshold=0.05
bash start.sh --dry-run                          # Print config, don't start
```
Pulls latest code, builds if needed, starts via `nohup`, verifies dashboard responds (HTTP 200).

### kill.sh — Stop all processes
```bash
bash kill.sh                                     # Graceful: SIGTERM → 5s wait → SIGKILL
bash kill.sh --quiet                             # No output (used by restart.sh)
bash kill.sh --cancel                            # Cancel CLOB orders (not yet implemented)
bash kill.sh --emergency                         # C2: Kill switch (see §7)
```

### restart.sh — Full restart cycle
```bash
bash restart.sh                                  # Stop → pull → rebuild → start
bash restart.sh --mode shadow                    # Restart in shadow mode
bash restart.sh --clean                          # Also purge stale cache files
bash restart.sh --set key=value                  # Forward overrides to start.sh
```

---

## 3. CLI Flags

```
prediction-trader [OPTIONS]

  -w, --workspace <PATH>     Workspace root [env: TRADER_WORKSPACE]
  -m, --mode <MODE>          shadow | live
  -p, --port <PORT>          Dashboard port (0 = disabled)
  -l, --log-level <LEVEL>    trace | debug | info | warn | error
  -s, --set <KEY=VALUE>      Override config (repeatable). See allowed keys below.
  -i, --instance <NAME>      Instance name (shadow-a, shadow-b, etc.)
  --dry-run                   Print resolved config and exit
  --no-pid-lock               Skip PID lock (for multi-instance)
```

**Allowed `--set` keys**: `dashboard.port`, `mode`, `state.db_path`, `live_trading.shadow_only`, `live_trading.enabled`, `arbitrage.*` (capital_per_trade_pct, max_concurrent_positions, min_trade_size, max_position_size, max_days_to_resolution, min_profit_threshold, max_profit_threshold, replacement_cooldown_seconds, max_exposure_per_market, max_days_to_replacement, min_resolution_time_secs), `engine.*` (state_save_interval_seconds, monitor_interval_seconds, constraint_rebuild_interval_seconds), `monitoring.logging.level`.

All numeric values are bounds-checked (e.g., `capital_per_trade_pct` max 0.5, `max_concurrent_positions` max 200).

---

## 4. Dashboard

| Instance | Port | Access |
|----------|------|--------|
| Default | 5558 | `http://localhost:5558` |
| shadow-a | 5560 | `http://localhost:5560` |
| shadow-b | 5561 | `http://localhost:5561` |
| shadow-c | 5562 | `http://localhost:5562` |
| shadow-d | 5563 | `http://localhost:5563` |
| shadow-e | 5564 | `http://localhost:5564` |
| shadow-f | 5565 | `http://localhost:5565` |

**Remote access via SSH tunnel:**
```bash
ssh -L 5558:127.0.0.1:5558 vps-ubuntu
# Then open http://localhost:5558 in browser
```

**Features**: Open/closed positions, P&L tracking, portfolio charts, constraint/opportunity tables, system metrics (CPU/RAM/disk), WebSocket stats, latency histograms, circuit breaker status, kill switch button, POL gas balance.

---

## 5. Log Locations

| Log | Path |
|-----|------|
| Engine logs | `logs/supervisor-YYYY-MM-DD.log` (daily rotation) |
| Instance logs | `logs/{instance}/supervisor-{instance}-YYYY-MM-DD.log` |
| Start script | `logs/supervisor_start.log` |

**Config** (`config/config.yaml`):
```yaml
monitoring:
  logging:
    level: DEBUG
    retention: 30        # days
    rotation: daily
    log_dir: logs
    rust_file_prefix: rust_engine
```

**View logs:**
```bash
tail -f logs/supervisor-*.log                     # Latest log
tail -f logs/shadow-a/supervisor-shadow-a-*.log   # Instance-specific
```

**VPS logrotate** (`/etc/logrotate.d/prediction-trader`): daily, 7 rotations, compressed, 100M max.

---

## 6. Circuit Breaker (C1)

**Config** (`config/config.yaml → safety.circuit_breaker`):

| Param | Default | Meaning |
|-------|---------|---------|
| `max_drawdown_pct` | 0.10 | 10% from peak triggers trip |
| `max_consecutive_errors` | 3 | Error burst within window |
| `error_window_seconds` | 300 | 5-min sliding window |
| `api_timeout_seconds` | 600 | API unreachable timeout |

**Trip reasons**: Drawdown, ErrorBurst, ApiUnreachable, GasCritical.

**Check status:**
```bash
curl -s http://localhost:5558/state | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('circuit_breaker',{}))"
```
Or view on the dashboard header (shows trip reason + timestamp when tripped).

**Reset procedure**: Circuit breaker is **not persisted** — restart the process to clear it.
```bash
bash kill.sh && sleep 3 && bash start.sh
```
Fix the underlying issue first (top up POL, restore API connectivity, etc.).

---

## 7. Kill Switch (C2)

Two independent trigger paths:

### (a) Shell script
```bash
bash kill.sh --emergency
```
Writes `data/kill_switch.flag` → orchestrator detects within 1 tick → cancels CLOB orders → sets shadow mode → Telegram alert → graceful shutdown.

### (b) Dashboard button
Click **KILL SWITCH** in the dashboard header → confirmation dialog → POST `/api/kill-switch` → same actions as above (except process stays running in shadow mode).

**Actions on trigger**: Cancel all open CLOB orders (L2 auth stub until Milestone D), set `shadow_only = true`, send Telegram notification. Idempotent — safe to trigger multiple times.

---

## 8. Telegram Bot Setup

**Config** (`config/config.yaml → notifications`):
```yaml
notifications:
  enabled: true
  phone_number: '688371419'       # Telegram chat_id (not a phone number)
  rate_limit_seconds: 10
  on_entry: true
  on_resolution: true
  on_error: true
  on_circuit_breaker: true
  on_daily_summary: true
```

**Secrets** (`config/secrets.yaml`):
```yaml
telegram_bot_token: '<BOT_TOKEN>'
```

The webhook URL is auto-constructed: `https://api.telegram.org/bot<TOKEN>/sendMessage`.

**Get your chat_id**: Message `@userinfobot` on Telegram, or call `getUpdates` on the bot API.

**Events sent**: Position entry, resolution, proactive exit, errors, circuit breaker trip, daily P&L summary (midnight UTC), startup.

**Rate limiting**: 10s between messages. After 5 consecutive failures, 5-minute backoff.

**Test:**
```bash
curl -X POST "https://api.telegram.org/bot<TOKEN>/sendMessage" \
  -H "Content-Type: application/json" \
  -d '{"chat_id": "688371419", "text": "OPS_RUNBOOK test"}'
```

---

## 9. POL Gas Top-Up

**Wallet**: Address derived from `polymarket.private_key` in `secrets.yaml`. Funder address: `polymarket.funder_address`.

**Config** (`config/config.yaml → safety.gas_monitor`):
```yaml
safety:
  gas_monitor:
    enabled: true
    rpc_url: 'https://polygon-bor-rpc.publicnode.com'
    check_interval_seconds: 3600    # Hourly
    min_pol_balance: 1.0            # Warning threshold
    critical_pol_balance: 0.0       # Trip circuit breaker (set to 0 until funded)
```

**Check balance manually:**
```bash
curl -s -X POST https://polygon-bor-rpc.publicnode.com \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","method":"eth_getBalance","params":["<FUNDER_ADDRESS>","latest"],"id":1}'
```
Returns hex wei (1 POL = 1e18 wei).

**Top-up procedure**: Send POL to the funder address on Polygon mainnet (chain 137). Minimum recommended: 5 POL.

---

## 10. Backup & State Persistence

**State DB**: `data/state_rust.db` (or `data/system_state/execution_state_{instance}.db`)

| Mechanism | Detail |
|-----------|--------|
| In-memory SQLite | All reads/writes hit RAM — zero I/O latency |
| Disk mirror | Background thread, every 30s (`state_save_interval_seconds`) |
| `.bak` backup | Created before every `load_from_disk()` |
| Graceful shutdown | Final state flush before exit |

**Schema tables**: `scalars`, `positions`, `delay_p95`, `daily_reports`.

**Manual backup (pre-maintenance):**
```bash
cp -p data/state_rust.db data/state_rust.db.pre-maintenance-$(date +%s)
```

**Recovery from corruption:**
1. Restore from `.bak`: `cp data/.state_rust.db.bak data/state_rust.db`
2. Or delete DB entirely — system starts fresh with initial capital from config
3. Restart: `bash restart.sh`

---

## 11. Config File Structure

**Load order** (highest precedence first):
1. CLI flags (`--mode`, `--set`, `--port`)
2. Instance overlay (`config/instances/{name}.yaml`)
3. Local overlay (`config/config.local.yaml`) — per-machine, not in git
4. Main config (`config/config.yaml`)

| File | Purpose | In Git? |
|------|---------|---------|
| `config/config.yaml` | Base configuration | Yes |
| `config/config.local.yaml` | Per-machine overrides | No |
| `config/secrets.yaml` | Private keys, API tokens | No |
| `config/instances/{name}.yaml` | Instance parameter sets | Yes |

---

## 12. Monitoring Checklist

**Daily** (automated via Telegram C4):
- Daily P&L summary at midnight UTC: entries, exits, fees, net P&L, capital utilisation, drawdown

**Weekly**:
- Check VPS disk usage: `ssh vps-ubuntu 'df -h /'`
- Check VPS memory: `ssh vps-ubuntu 'free -h'`
- Review log retention (30-day auto-cleanup)

**Monthly**:
- Verify POL gas balance
- Check ZAP-Hosting dashboard / billing

**Quarterly**:
- Log in to ZAP-Hosting dashboard to prevent account dormancy
