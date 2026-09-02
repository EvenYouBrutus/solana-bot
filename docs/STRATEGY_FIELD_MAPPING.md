# Strategy Field Mapping — Historical Backtest Requirements

This document maps every input the production strategy reads at entry time to
the field required in `HistoricalSignal`, the PIT (point-in-time) requirement,
the historical data source, and whether the data is currently available.

## A. Production Strategy Entry Decision (in `evaluate_signal`)

The production entry path reads from the following inputs in this order:

### 1. `MarketSnapshot` fields (from `src/domain/market.rs`)

| Field | Used as | PIT requirement | Source | Currently available? |
|---|---|---|---|---|
| `mint` | Consistency with `signal.mint` | n/a | RPC / on-chain | Yes (RPC) |
| `observed_at` | Staleness check vs `now` | `observed_at <= signal_timestamp` | RPC / indexer | **No** — no historical data source |
| `liquidity_usd` | Reject if `< config.risk.min_liquidity_usd` | `<= signal_timestamp` | DEX pool reserves | **No** — no historical data source |
| `volatility_pct` | `risk_score = 100 - min(volatility, 100)` | `<= signal_timestamp` | On-chain price history | **No** |
| `buy_sell_imbalance` | `momentum_score = clamp(bi*50, 0, 100)` | `<= signal_timestamp` | DEX activity | **No** |
| `volume_24h_usd` | Not used by strategy | n/a | DEX volume | n/a |
| `price_usd` | Not used by strategy gate; used by backtest as entry price | `<= signal_timestamp` | DEX price | **No** |
| `received_at`, `slot` | Not used by strategy | n/a | RPC | n/a |

### 2. `TokenSafety` fields (from `src/domain/token.rs`)

| Field | Used as | PIT requirement | Source | Currently available? |
|---|---|---|---|---|
| `observed_at` | Staleness check vs `now` | `<= signal_timestamp` | On-chain account state | **No** |
| `token_age_secs` | `>= min_token_age_secs` (86400 = 1 day) | `<= signal_timestamp` | On-chain mint timestamp | **Yes** (RPC, but not historically) |
| `mint_authority_present` | Reject if `true` | `<= signal_timestamp` | Mint account info | **Yes** (RPC) |
| `freeze_authority_present` | Reject if `true` | `<= signal_timestamp` | Mint account info | **Yes** (RPC) |
| `holder_top10_pct` | Reject if `> 70` | `<= signal_timestamp` | Token largest accounts | **Yes** (RPC, but not historical) |
| `sellable` | Reject if `!= Some(true)` | `<= signal_timestamp` | DEX route existence | **Partially** (live DEX check, no historical) |
| `route_available` | Reject if `!= Some(true)` | `<= signal_timestamp` | DEX route existence | **Partially** (live DEX check, no historical) |
| `creator_suspicious` | Reject if `== Some(true)` | `<= signal_timestamp` | External (Helius / Birdeye) | **No** |
| `abnormal_activity` | Reject if `== Some(true)` | `<= signal_timestamp` | External | **No** |
| `liquidity_change_pct` | Reject if `< -20` | `<= signal_timestamp` | On-chain LP event log | **No** |
| `liquidity_locked_or_burned` | Not read by strategy | n/a | LP lock events | n/a |

### 3. `WalletStats` fields (from `src/domain/wallet.rs`) — PIT-CRITICAL

The entry decision reads the following per-wallet:

| Field | Used as | PIT requirement | Source | Currently available? |
|---|---|---|---|---|
| `wallet` | String ID in TradeSignal | n/a | RPC / indexer | Yes (RPC) |
| `score` | Reject if `< min_wallet_score` (60.0) | `<= signal_timestamp` | Historical trade replay | **No** — no infrastructure to reconstruct historical performance |
| `trades` | Reject if `< min_wallet_samples` (25) | `<= signal_timestamp` | Historical trade replay | **No** |
| `updated_at` | Reject if `> now` | `<= signal_timestamp` | Snapshot timestamp | **Partially** — can be set if wallet tracker exists |
| `recent_return_pct` | Mean into `wallet_recent_score` | `<= signal_timestamp` | Historical trade replay | **No** |
| `realized_pnl_usd, win_rate, avg_return_pct, median_return_pct, max_drawdown_pct, concentration_pct, scam_exposure_pct, tier` | Not read by strategy gate | `<= signal_timestamp` | Historical trade replay | **No** |
| `entity_id` | Not read by strategy | `<= signal_timestamp` | Manual | n/a |

**These are the most critical missing inputs.** The smart-money signal layer has NO
historical reconstruction capability in the codebase. The current `collect_live`
path in `src/collector/mod.rs` synthesizes fake wallet IDs and `score` derived from
`price_impact_bps` (line 195: `< 50 bps → 85, < 100 → 75, else → 65`). This is
NOT historical evidence — it is a heuristic. The `WalletTracker` in
`src/smart_money/tracker.rs` only tracks wallets seen during the current session.

### 4. `ExpectedValue` (from `src/economics/cost_model.rs`)

| Field | Used as | PIT requirement | Source | Currently available? |
|---|---|---|---|---|
| `net_return_pct` | Reject if `< config.economics.min_expected_net_return_pct` (2.0%) | `<= signal_timestamp` | Computed from `CostModel` | **Derivable from cost model** |

`net_return_pct` is computed by the backtest from the point-in-time cost model using
`ExpectedValue::estimate(required_avg_win_pct, ...)`. This is reconstructable from
`signal.costs` alone. **Not a missing input.**

### 5. `CostModel` (from `src/economics/cost_model.rs`)

| Field | Used as | PIT requirement | Source | Currently available? |
|---|---|---|---|---|
| `observed_at` | PIT check (in `simulate_signal`) | `<= signal_timestamp` | Same as signal_timestamp | Yes (set in collector) |
| `input.position_size_usd` | Must equal `position_usd` (collector checks this) | `<= signal_timestamp` | Config | Yes |
| `input.avg_priority_fee_usd, avg_swap_fee_bps, avg_slippage_bps, avg_price_impact_bps, failed_tx_rate, avg_failed_tx_cost_usd, assumed_win_loss_ratio, assumed_avg_loss_pct` | All used in `EconomicGate.check()` and `ExpectedValue::estimate()` | `<= signal_timestamp` | Historical fee observations | **Partially** — calibrated by the operator, not historical observation |

### 6. `signal.expected_gross_return_pct`

The production strategy reads this from the `CandidateInput`, and the runtime
passes it to `evaluate_signal` (see `src/runtime.rs`). The **backtest explicitly
does NOT use it** in entry — it reconstructs the expected return from the cost model
(`required_avg_win_pct`). This is by design (avoid look-ahead bias). **Not a missing
input for the backtest.**

## B. Backtest Exit Simulation (not entry, but needed for results)

The backtest also requires `price_history: Vec<PriceObservation>` of **subsequent
observations** to simulate exits:

| Field | Used as | PIT requirement | Source | Currently available? |
|---|---|---|---|---|
| `price_history[i].timestamp` | Entry timestamp + walk | `>= signal_timestamp` | Historical DEX price feed | **No** — no historical price source |
| `price_history[i].price_usd` | Exit SL/TP/trailing checks | `<= signal_timestamp + max_holding` | Historical price | **No** |
| `price_history[i].liquidity_usd` | Liquidity exit check | `<= signal_timestamp + max_holding` | Historical DEX pool state | **No** |

Temporal resolution must be fine enough that no interval's range contains both the
SL price and the TP price simultaneously (otherwise the trade is marked ambiguous).
With SL=5% and TP=12% (from paper.toml), the gap is 17%. **Candle intervals narrower
than 17%/max_move** are required. On Solana memecoin tokens, 1-minute candles
typically suffice.

## C. Summary: What's Missing for a Real Backtest

| Category | Status |
|---|---|
| RPC infrastructure for current state | **Yes** (in `src/data/rpc.rs`) |
| RPC infrastructure for historical state | **No** (no `getSignaturesForAddress`, `getMultipleAccounts`, `getTokenLargestAccounts` at a historical slot, etc.) |
| Historical DEX price feed | **No** (no Birdeye / Bitquery / Helius DAS / Geyser integration) |
| Historical DEX liquidity snapshot | **No** |
| Historical token holder distribution at a slot | **No** |
| Historical wallet PnL reconstruction | **No** (no `WalletTracker` historical persistence) |
| Historical safety features (`creator_suspicious`, `abnormal_activity`, `liquidity_change_pct`) | **No** |
| Historical price observation (post-signal) for exit simulation | **No** |
| Strategy override of the entry decision | **Yes** (already a PIT-faithful `evaluate_signal_pit` clone) |

## D. The Actual Blocker

The codebase is **architecturally ready** for a real backtest:
- The `HistoricalSignal` schema is complete.
- The PIT validation is strict (fail-closed on look-ahead bias).
- The production entry decision has a PIT-faithful replica.
- The exit simulation is deterministic and uses the same `exit_reason()` as production.
- The statistics layer has OOS-aware CI verdict.

The blocker is **data acquisition, not code**:
1. The `RpcPool` only implements forward-looking methods. There is no historical
   RPC or external price feed integration.
2. The `WalletTracker` only tracks wallets seen during the current session. There
   is no historical smart-money reconstruction.
3. The `Collector` is forward-looking. It does not write historical data to disk
   in a way that could be re-played later.

Until historical data sources are wired in, the bundled `data/sample_historical.jsonl`
is the only thing the backtest can run on, and it is explicitly synthetic.
Changing `is_synthetic_data = true` to `false` on a synthetic file would constitute
fraud, and the codebase's existing fail-closed design correctly prevents that.
