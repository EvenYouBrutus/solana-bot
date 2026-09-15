# Solana smart-money bot

A conservative Rust trading-system foundation for smart-money copy-trading on Solana. It is **not proven profitable** and must not be funded on the basis of this repository or a backtest alone.

## What is implemented

### Core pipeline

Timestamped candidate record → strict token/wallet/market strategy gates → conservative round-trip economics → risk authorization → position sizing → fresh Jupiter quote → idempotent order reservation → versioned (V0) transaction execution → confirmed fill/position persistence. Paper and live use the same gates; only execution differs.

### Operating modes

| Mode | Description |
|------|-------------|
| `replay` | Static JSONL feed, deterministic replay, no broadcast |
| `paper` | Real Jupiter quotes, simulated fills, no signing/broadcast |
| `live` | Full execution with keypair signing and on-chain submission |

### Risk controls (all enforced before every entry)

- **Aggregate exposure cap** — `max_live_capital_usd` across all open positions
- **Per-position equity cap** — `max_position_percent_of_equity`
- **Per-position liquidity cap** — `max_position_percent_of_liquidity`
- **Concurrent position limit** — `max_concurrent_positions`
- **Daily trade count limit** — `max_trades_per_day`
- **Daily drawdown limit** — `max_daily_loss_pct` trips kill switch
- **Kill switch on consecutive failures** — after `max_consecutive_failures`
- **Cooldown after loss** — `cooldown_after_loss_secs` blocks entries
- **Pre-trade slippage check** — `max_slippage_bps`
- **Pre-trade price impact check** — `max_price_impact_bps`
- **Position sizing** — `max_risk_per_trade_percent / stop_loss_pct` enforces maximum position USD
- **Exits always allowed** — `authorize_exit` never checks kill switch, cooldown, or daily loss

### Execution policy

- Versioned transactions (V0) with Address Lookup Table resolution
- Payer validation: sole signer must be the configured wallet
- Program allowlist: every invoked program must be in `execution.allowed_program_ids`
- ALT index bounds validated before account resolution

### Exit management

Independent exit monitor handles all exit reasons: StopLoss, TakeProfit, TrailingStop, TimeLimit, LiquidityDeterioration, SignalInvalidated. Runs as a separate tokio task, evaluates positions every tick.

### Startup validation (live mode)

- Keypair existence and validity from configured environment variable
- Jupiter API key presence check
- SOL balance minimum (0.1 SOL) for execution fees
- Signer cross-session consistency check against persisted state
- Kill switch and incomplete order state check
- RPC endpoint count warning (< 2)

### Status command

```bash
cargo run -- status --config config/live.toml           # human-readable
cargo run -- status --config config/live.toml --format json  # JSON output
```

Displays: mode, wallet address, SOL balance, open positions, aggregate exposure, equity, daily PnL, kill switch state, emergency stop state, unresolved orders, RPC health, last fill.

### Reconciliation

- Startup reconciliation of all incomplete orders
- Periodic reconciliation at configurable intervals
- Final reconciliation on shutdown
- Exit monitor also reconciles stale orders
- On-chain swap outcome extraction with atomic fill persistence

### State persistence (SQLite WAL + synchronous=FULL)

- Kill switch reason
- Emergency stop state
- Orders with idempotency keys
- Fills with atomic multi-step persistence
- Positions and portfolio state
- Session state (day, equity, trade count)
- KV store for runtime metadata

### Observability

Structured logging with `tracing` using per-pipeline `info_span!` containing `mint`, `signal_id`, `order_id`, and `position_id` fields. Supports JSON output mode via `observability.json_logging = true`. RPC health checks report latency, status codes, and errors per endpoint.

### Security

- No private keys/secrets logged, printed, or stored in plaintext
- Environment variables used exclusively for keypair loading
- Only public key (signer pubkey) is logged
- Mutex locks use `expect()` with descriptive messages
- Division-by-zero guards on all non-trivial arithmetic paths
- No `unsafe` blocks in the codebase

## Setup

Install a current Rust toolchain, copy the configuration, and configure at least two independent RPC endpoints:

```bash
cp config.example.toml config/local.toml
cargo run -- check --config config/local.toml
```

For live trading:

```bash
cp config/live.toml config/local.toml  # edit as needed
export SOLANA_BOT_KEYPAIR_JSON='[64 byte-array values]'
export JUPITER_API_KEY='your-api-key'
cargo run -- check --config config/local.toml
cargo run -- run --config config/local.toml
```

Never put secrets in TOML, Git, logs, or a database. Review every allowlisted program for the specific Jupiter routes you permit.

## Backtest

```bash
cargo run -- backtest \
  --config config/local.toml \
  --bt-config config/backtest.toml \
  --input data/sample_historical.jsonl
```

Deterministic OHLC-aware backtest with train/validation/OOS splitting, bootstrap confidence intervals, and exclusion of censored/ambiguous trades from statistics.

## Operations

```bash
cargo run -- status --config config/live.toml              # live state
cargo run -- reconcile --config config/local.toml          # reconcile orders
cargo run -- emergency-stop --config config/local.toml     # halt entries
cargo run -- clear-emergency-stop --config config/local.toml  # resume entries
cargo run -- exit-all --config config/local.toml           # force exit positions
cargo run -- report --config config/local.toml             # session report
```

## Pre-deployment checklist

```bash
cargo fmt --check
cargo clippy -- -D warnings
cargo test
cargo build --release
```

**409 tests** covering risk controls, execution policy, reconciliation, exit logic, portfolio accounting, backtesting, and failure injection.

No statistically meaningful out-of-sample test with realistic fees, liquidity, failures, latency, and adverse execution is included. Accordingly, this strategy has **not demonstrated positive out-of-sample expectancy after realistic costs**.
