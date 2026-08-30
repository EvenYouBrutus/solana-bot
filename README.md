# Solana smart-money bot

This is a conservative Rust trading-system foundation. It is **not proven profitable** and must not be funded on the basis of this repository or a backtest alone.

## What is implemented

The executable uses one fail-closed path: timestamped candidate record → strict token/wallet/market strategy gates → conservative round-trip economics → risk authorization → fresh Jupiter quote → idempotent order reservation → execution → confirmed fill/position persistence. Paper and live use the same gates; only execution differs.

SQLite uses WAL plus synchronous commits and persists observations, orders, fills, positions, idempotency keys, and a latched kill switch. At startup, a persisted kill switch or any pending/submitted/unknown order blocks new entries. An unknown send is never automatically resent.

Live Jupiter transactions are requested as legacy transactions, parsed before signing, require exactly the configured wallet as sole signer, and every invoked program must be in `execution.allowed_program_ids`. Address-lookup-table transactions are rejected rather than being signed without complete account resolution.

## Important limitations

This repository does **not** yet include a verified historical-indexer/DEX collector, reliable holder/liquidity-lock enrichment, WebSocket ingestion, automated exit scheduler, or a calibrated replay data set. `runtime.signal_feed_path` is therefore an explicit JSONL boundary for such a collector. Records must contain all required safety, market, wallet, and cost evidence; absent/uncertain fields are rejected. These limitations mean the project must not be described as production-ready or as an autonomous smart-money bot.

Paper mode fetches real Jupiter quotes but does not sign or broadcast. Replay currently reuses the paper executor over a static chronological JSONL feed; it is not a complete realistic backtester. Do not infer expected returns from the `expected_gross_return_pct` input: it is an externally supplied hypothesis that is cost-gated, not proof.

## Setup

Install a current Rust toolchain, copy the configuration, and configure at least two independent RPC endpoints for any serious operation:

```bash
cp config.example.toml config/local.toml
cargo run -- check --config config/local.toml
```

`[runtime].signal_feed_path` points to newline-delimited `CandidateInput` JSON records (defined in `src/runtime.rs`). Each record includes observed/received timestamps, wallet statistics as-of the observation time, token safety evidence, a market snapshot, and a `CostModel`. The `position_usd` and the cost model’s `position_size_usd` must match.

## Modes

Replay (static feed; no broadcast):

```bash
cargo run -- run --config config/replay.toml
```

Paper (real quote, simulated fill):

```bash
cargo run -- run --config config/paper.toml
```

Live requires `mode = "live"`, a positive conservative `max_live_capital_usd` no greater than starting capital, a non-empty reviewed program allowlist, and a keypair only through the configured environment variable:

```bash
export SOLANA_BOT_KEYPAIR_JSON='[64 byte-array values]'
cargo run -- check --config config/live.toml
cargo run -- run --config config/live.toml
```

Never put that value in TOML, Git, logs, or a database. Review every allowlisted program for the specific Jupiter routes you permit. The safest default is an empty allowlist, which prevents live mode from starting.

## Operations and validation

Use a persistent database path. To stop new entries, persist the kill-switch key through the operational control plane (the current binary has no CLI command to clear it); restart will preserve the block. Resolve any unknown transaction signature manually through multiple RPCs before resuming.

Run the following before deployment:

```bash
cargo fmt --check
cargo clippy -- -D warnings
cargo test
cargo build --release
```

No statistically meaningful out-of-sample test with realistic fees, liquidity, failures, latency, and adverse execution is included. Accordingly, this strategy has **not demonstrated positive out-of-sample expectancy after realistic costs**.
