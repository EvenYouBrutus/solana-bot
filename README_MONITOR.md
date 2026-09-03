# Smart Money Monitor

Python script that tracks high-performing Solana wallets in real-time and outputs `CandidateInput` JSONL records for the Rust trading bot.

## What It Does

1. **Profiles wallets** by analyzing the last 100 historical transactions (win rate, PnL, return distribution).
2. **Listens** to live WebSocket logs for those wallets.
3. **Detects buys** on Raydium/Jupiter DEX programs.
4. **Runs safety checks**: mint/freeze authority, token age, liquidity, holder concentration.
5. **Computes consensus**: requires N qualified wallets (score >= 60) buying the same token.
6. **Outputs** one JSONL line per valid signal to `signals/live_signals.jsonl`.

## Setup

```bash
pip install -r requirements.txt
cp .env.example .env
# Edit .env with your RPC URLs and Birdeye API key
# Edit smart_wallets.txt with real wallet addresses (one per line)
```

## Running

```bash
# Default: 100k SOL position, signals to signals/live_signals.jsonl
python monitor.py

# Custom position size
python monitor.py --position-usd 10000

# Custom output path
python monitor.py --output signals/custom.jsonl
```

## Connecting to the Rust Bot

Point the Rust bot at the monitor's output file:

```toml
# In config/local.toml
[runtime]
signal_feed_path = "signals/live_signals.jsonl"
```

Then run the bot in replay mode:

```bash
cargo run -- run --config config/local.toml
```

## Configuration

Environment variables (`.env`):

| Variable | Required | Description |
|---|---|---|
| `SOLANA_RPC_URL` | Yes | HTTP RPC for transaction history |
| `SOLANA_WSS_URL` | Yes | WebSocket RPC for live logs |
| `BIRDEYE_API_KEY` | Yes | OHLCV / price data |
| `HELIUS_API_KEY` | No | Used for some RPC calls |

## Testing

Offline tests (no API keys needed):

```bash
python monitor_tests.py
```

## Safety

- Read-only: never signs transactions or holds private keys.
- All signals pass through mint authority, freeze authority, liquidity, and token age gates before output.
- Invalid or duplicate mints are filtered within each session.
