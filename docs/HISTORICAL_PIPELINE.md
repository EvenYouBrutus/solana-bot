# Real Historical Data Pipeline

This module ingests **real** Solana historical data for the backtest engine:

- **OHLCV** — Birdeye-compatible REST provider (default
  `https://public-api.birdeye.so`).
- **Wallet activity** — Solana RPC (`getSignaturesForAddress` →
  `getTransaction`) reconstructed point-in-time.
- **Token safety** — Solana RPC `getAccountInfo` for the mint at
  signal time.
- **Cost model** — observed market dispersion → slippage/impact, with
  calibrated defaults (always `MODELED`).

The output is `data/historical_real.jsonl`, one
`src/backtest/data.rs::HistoricalSignal` per line. No synthetic data is
mixed in; the existing `data/sample_historical.jsonl` (with
`is_synthetic_data = true` in `config/backtest.toml`) remains as a
mechanics-test fixture.

## Required environment variables

Set these before running `historical-build`:

```text
BIRDEYE_API_KEY      OHLCV provider key (sent as X-API-KEY)
SOLANA_RPC_URL       Solana JSON-RPC endpoint URL (Helius recommended)
HELIUS_API_KEY       Used to auto-derive SOLANA_RPC_URL when not set
```

Override knobs:

```text
OHLCV_PROVIDER_URL   Override the OHLCV endpoint
OHLCV_CACHE_DIR      Override the cache directory (default: data/historical_cache)
```

Never commit the values; they belong in a local `.env` file.

## Commands

```bash
cargo run --release -- historical-build \
  --start  2024-04-01T00:00:00Z \
  --end    2024-12-01T00:00:00Z \
  --mints  DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263,So11111111111111111111111111111111111111112 \
  --interval 1h \
  --signals-per-mint 5 \
  --output data/historical_real.jsonl
```

```bash
cargo run --release -- historical-validate \
  --input data/historical_real.jsonl
```

```bash
cargo run --release -- backtest \
  --config config/local.toml \
  --bt-config config/backtest-real.toml \
  --input data/historical_real.jsonl \
  --format text \
  --trades data/backtest_trades.json
```

## Point-in-time guarantees

Every record carries an `observed_at` / `updated_at` / `block_time` on
every decision-time field. The validator and the backtest engine both
refuse any record whose decision-time data is dated after
`signal_timestamp`. Future trades never influence a signal.

## Fail-closed behavior

- If the OHLCV provider is unreachable, the affected signal is skipped
  (the pipeline continues with the rest of the dataset).
- If wallet reconstruction fails for a wallet, that wallet is omitted;
  the signal is rejected by the engine if fewer than
  `min_consensus_wallets` remain.
- If a token-safety field cannot be reconstructed historically, it is
  `None`. The engine rejects signals with `sellable = None` /
  `route_available = None` — fail-closed.

## Resumability

`historical-build` writes a `<output>.resume.json` file that records
every signal written. A re-run skips already-written signals, so a
partially-completed dataset can be resumed by re-invoking the same
command.

## Limitations of this iteration

- Wallet PnL is reconstructed from chain signatures only; transaction
  body parsing for notional is intentionally not implemented. Wallets
  with on-chain history will have `trades > 0` and a tier assigned by
  the production scoring function, but `realized_pnl_usd` is a
  conservative placeholder.
- `holder_top10_pct`, `sellable`, `route_available`,
  `creator_suspicious`, `abnormal_activity`, and `liquidity_change_pct`
  are returned as `None` because the RPC pool does not implement
  `getTokenLargestAccounts` at historical slots or external threat-intel
  lookups. The dataset records `None` instead of fabricating values.
- Historical liquidity snapshots are not provided by the OHLCV endpoint;
  `liquidity_usd` on `PriceObservation` is `0` when missing.