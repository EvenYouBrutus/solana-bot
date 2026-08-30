# Solana smart-money bot

This is a fail-closed Rust trading system foundation for following independently corroborated, historically qualified Solana wallet flow. It is not a promise of profitability and must not be pointed at capital until replay, paper, and small-capital live results demonstrate an edge after costs.

## Architecture

`data` owns HTTP RPC failover and observation timestamps. `smart_money` scores wallet histories without using future observations. `strategy` combines qualified-wallet consensus with market/safety/economic gates. `economics` estimates all round-trip costs and uncertainty haircuts. `risk` is the final entry authority. `execution` isolates Jupiter/RPC swaps behind a trait; `storage` persists SQLite state and idempotency keys; `portfolio` only accepts confirmed fills.

Every candidate must have fresh data, two or more qualified wallets, a safe token profile, configured liquidity, a positive net expected return beyond the configured margin, and pass risk checks. Missing or stale data rejects the trade.

## Strategy and scoring

A wallet starts as `Candidate`, becomes `Observed` after 5 trades, and can become `Qualified` after 25 trades and score >= 60; `HighConfidence` is score >= 75. The conservative 0–100 score is sample-size-capped and rewards win rate, mean/median/recent returns and realized PnL while penalizing drawdown, concentration, and scam exposure. This must be populated from historical, point-in-time observations—not future realized outcomes.

Entry requires at least two independent qualified wallets buying the same token, token age >= configured minimum, no observed mint/freeze authority, top-10 holder concentration <= 70%, minimum liquidity, fresh data, a signal confidence >= threshold, and expected net return above the cost/uncertainty threshold. The implementation rejects rather than imputes unavailable values.

Exit rules are explicit: liquidity deterioration or signal invalidation first, then stop loss, take profit, trailing stop from high-water mark, and maximum holding time. A production scheduler must feed real prices/liquidity to these rules and execute the resulting exit through the same executor.

## Risk and operations

Risk enforces concurrent positions, daily count, equity/liquidity position caps, slippage, daily loss, loss cooldown, and a latched drawdown kill switch. The latch is persisted and cannot be cleared by restart. SQLite has a unique idempotency key per order; a submitted/unknown transaction must be reconciled by signature before any retry.

Copy the example config, use independent production RPC providers, and keep secrets out of files:

```bash
cp config.example.toml config/local.toml
cargo run -- check --config config/local.toml
cargo run -- run --config config/local.toml
```

For live mode change `mode = "live"` and provide a JSON byte-array keypair only via the configured environment variable:

```bash
export SOLANA_BOT_KEYPAIR_JSON='[...64 bytes...]'
cargo run -- check --config config/live.toml
cargo run -- run --config config/live.toml
```

Live operation should begin with the example $25 cap or less. Require multiple healthy RPC sources, verified wallet balance, a tested kill-switch runbook, alerting on stale data and submitted-but-unconfirmed orders, and a signed review of all strategy calibration. Never scale capital from a backtest alone. Replay must preserve observation time, liquidity, quote age, fill delay, failed sends, fees, slippage, price impact and exit costs, then report out-of-sample net return, drawdown, expectancy, profit factor, exposure, turnover, holding time, and sensitivity to worse execution.

## Verification

Run `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test`. The current execution integration uses Jupiter's quote and swap endpoints and confirms signatures through RPC before reporting a fill. Provider-returned transactions require an explicit program/instruction allowlist review before enabling live funds; do not treat provider availability as a substitute for transaction-policy validation.
