# SETUP GUIDE
# Prediction Market Trading System

## Environment

- **WSL** (Ubuntu) on Windows
- **Virtual environment:** `~/prediction-trader-env`
- **Project location:** `/home/andydoc/gdrive_openclaw/prediction-trader/`
- **Google Drive mount:** via rclone (`gdrive_openclaw`)

---

## Starting the System

    ~/start-prediction-trader.sh

**Note:** The start script opens the browser *before* activating the venv — this is intentional. Activating the venv breaks WSL interop, so wslview must run first.

This starts all 4 layers and the live dashboard automatically.

- Dashboard opens in your browser at **http://localhost:5555**
- Auto-refreshes every 5 seconds
- Survives Ctrl+C — keeps running between restarts

**To stop trading but keep dashboard running:** Ctrl+C

**To stop everything including dashboard:**

    kill %1

Or just close the terminal.

---

## Directory Structure

    /home/andydoc/gdrive_openclaw/prediction-trader/
    ├── config/
    │   └── config.yaml                  <- All system configuration
    ├── layer1_market_data/
    │   ├── market_data.py               <- Polymarket API collector
    │   └── data/                        <- Live market cache
    ├── layer2_constraint_detection/
    │   ├── constraint_detector.py       <- Finds market relationships
    │   └── data/
    │       └── latest_constraints.json  <- 110 active constraints
    ├── layer3_arbitrage_math/
    │   ├── arbitrage_engine.py          <- Calculates arb opportunities
    │   └── data/                        <- Saved opportunity files
    ├── layer4_execution/
    │   └── execution_engine.py          <- Position management
    ├── logs/
    │   └── trading_system_YYYYMMDD.log  <- Daily log files
    ├── data/
    │   └── system_state/                <- Persisted positions & metrics
    ├── main.py                          <- Main orchestrator + dashboard
    ├── paper_trading_complete.py        <- Paper trading engine
    ├── requirements.txt                 <- Python dependencies
    └── setup.ps1                        <- Windows setup (legacy)

---

## How It Works

1. **Layer 1 - Market Data** polls Polymarket every 30s, loads 1000 markets
2. **Layer 2 - Constraint Detection** finds mutually exclusive market groups (110 constraints)
3. **Layer 3 - Arbitrage Math** scans constraints for price sum < 1.0 (guaranteed profit)
4. **Layer 4 - Execution** validates and paper-trades top opportunities

### Deduplication
After a position closes, the same markets are on a **5-minute cooldown** before
being traded again. The system checks the top 15 opportunities per cycle so it
moves on to the next best trade after skipping recent ones.

---

## Dependencies

Install into the virtual environment:

    source ~/prediction-trader-env/bin/activate
    pip install -r requirements.txt

Key packages: `aiohttp`, `cvxpy`, `flask`, `loguru`, `numpy`, `pandas`, `pyyaml`

Full pinned versions are in `requirements.txt`.

---

## Dashboard

Built into `main.py` — no separate process needed.

- Starts automatically with the trading system
- **http://localhost:5555**
- Shows: capital, return %, trade count, win rate, open/closed positions,
  markets loaded, constraints, arb opportunities, recent activity, live logs
- If already running on restart, the existing instance is reused (no duplicate tabs)

---

## Resetting the System

To start fresh (clears all positions, trade history and resets capital to the configured starting amount):

    rm -f /home/andydoc/gdrive_openclaw/prediction-trader/data/system_state/execution_state.json
    ~/start-prediction-trader.sh

---

## Checking Performance

    # Tail the live log
    tail -f /home/andydoc/gdrive_openclaw/prediction-trader/logs/trading_system_$(date +%Y%m%d).log

    # Check saved execution state
    cat /home/andydoc/gdrive_openclaw/prediction-trader/data/system_state/execution_state.json

Or just watch the dashboard at http://localhost:5555.

---

## Troubleshooting

**Port 5555 already in use (not from this system):**

    lsof -i :5555

# To manually open the dashboard:
    wslview http://127.0.0.1:5555