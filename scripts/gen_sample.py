#!/usr/bin/env python3
"""Generate the bundled sample_historical.jsonl for the backtest integration test.

This dataset is SYNTHETIC / TEST DATA. It must never be presented as
evidence that the real strategy is profitable.

It covers:
- Train / Validation / OOS splits
- Accepted signals with exit types: TP, SL, trailing stop, liquidity,
  time limit
- Ambiguous (would-be) trade
- Censored trade (insufficient future data)
- Strategy-rejected signal (low wallet score)
- Future-dated invalid data (PIT violation at load time)
- Positive OOS result and negative OOS result
"""
import json
import sys


def make_signal(
    signal_timestamp,
    mint,
    price,
    liquidity,
    wallets,
    price_history,
    *,
    volatility=12,
    imbalance=0.70,
    volume=80000,
    holder_top10=35,
    token_age=259200,
    sellable=True,
    route_available=True,
    source="synthetic_test",
    market_observed_at=None,
    safety_observed_at=None,
    costs_observed_at=None,
    position_usd="4",
    expected_gross_return_pct="15",
    failed_tx_rate="0.05",
    failed_tx_cost_usd="0.002",
    swap_fee_bps="30",
    slippage_bps="50",
    impact_bps="20",
    priority_fee_usd="0.002",
):
    if market_observed_at is None:
        market_observed_at = signal_timestamp
    if safety_observed_at is None:
        safety_observed_at = signal_timestamp
    if costs_observed_at is None:
        costs_observed_at = signal_timestamp
    return {
        "signal_timestamp": signal_timestamp,
        "mint": mint,
        "market": {
            "mint": mint,
            "price_usd": str(price),
            "liquidity_usd": str(liquidity),
            "volume_24h_usd": str(volume),
            "volatility_pct": str(volatility),
            "buy_sell_imbalance": str(imbalance),
            "observed_at": market_observed_at,
            "received_at": market_observed_at,
        },
        "safety": {
            "mint_authority_present": False,
            "freeze_authority_present": False,
            "holder_top10_pct": str(holder_top10),
            "token_age_secs": token_age,
            "sellable": sellable,
            "route_available": route_available,
            "observed_at": safety_observed_at,
        },
        "wallets": wallets,
        "costs": {
            "observed_at": costs_observed_at,
            "input": {
                "position_size_usd": position_usd,
                "avg_priority_fee_usd": priority_fee_usd,
                "avg_swap_fee_bps": swap_fee_bps,
                "avg_slippage_bps": slippage_bps,
                "avg_price_impact_bps": impact_bps,
                "failed_tx_rate": failed_tx_rate,
                "avg_failed_tx_cost_usd": failed_tx_cost_usd,
                "assumed_win_loss_ratio": "2",
                "assumed_avg_loss_pct": "10",
            },
            "source": source,
            "is_live_snapshot": False,
        },
        "position_usd": position_usd,
        "expected_gross_return_pct": expected_gross_return_pct,
        "token_decimals": 6,
        "base_mint_decimals": 9,
        "price_history": price_history,
    }


def wallet(wallet_id, score, ts_update, trades=100):
    return {
        "wallet": wallet_id,
        "realized_pnl_usd": "5000",
        "win_rate": "0.72",
        "avg_return_pct": "18",
        "median_return_pct": "14",
        "max_drawdown_pct": "15",
        "trades": trades,
        "recent_return_pct": "12",
        "concentration_pct": "4",
        "scam_exposure_pct": "0",
        "score": str(score),
        "tier": "Qualified",
        "updated_at": ts_update,
    }


def obs(timestamp, price, liquidity=150000):
    # Normalize price: strip trailing zeros for compactness.
    if isinstance(price, float):
        price_s = f"{price:.10f}".rstrip("0").rstrip(".")
    else:
        price_s = str(price)
    return {
        "timestamp": timestamp,
        "price_usd": price_s,
        "liquidity_usd": str(liquidity),
    }


# Entry price: 0.000100
# SL at -5%: 0.000095
# TP at +12%: 0.000112
# Trailing stop: 4% from high-water
ENTRY = "0.000100"
SL = "0.000095"
TP = "0.000112"

# Two wallets with high scores ensure the signal score passes (>= 65).
# The score breakdown with score=85,82; buy_sell=0.70; vol=12; liq=150000:
# wallet_score=83.5, consensus=40, liquidity=100, momentum=35,
# risk=88, economic~=50.5  -> mean ~66.2 (above 65)
ACCEPTED_WALLETS_TRAIN = [
    wallet("0xTRAIN1", 85, "2024-01-15T11:50:00Z"),
    wallet("0xTRAIN2", 82, "2024-01-15T11:55:00Z"),
]
ACCEPTED_WALLETS_VAL = [
    wallet("0xVAL1", 85, "2024-06-15T09:50:00Z"),
    wallet("0xVAL2", 82, "2024-06-15T09:55:00Z"),
]
ACCEPTED_WALLETS_OOS_POS = [
    wallet("0xOOS_P1", 85, "2024-09-10T08:50:00Z"),
    wallet("0xOOS_P2", 82, "2024-09-10T08:55:00Z"),
]
ACCEPTED_WALLETS_OOS_NEG = [
    wallet("0xOOS_N1", 85, "2024-10-25T14:50:00Z"),
    wallet("0xOOS_N2", 82, "2024-10-25T14:55:00Z"),
]

records = []

# ---------------------------------------------------------------------------
# TRAIN split (before 2024-06-01)
# ---------------------------------------------------------------------------

# 1. Train - TP (Jan 15, 2024)
records.append(make_signal(
    "2024-01-15T12:00:00Z",
    "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263",
    0.000100, 150000,
    ACCEPTED_WALLETS_TRAIN,
    [
        obs("2024-01-15T12:05:00Z", 0.000112),  # +12% -> TP
    ],
))

# 2. Train - SL (Feb 10, 2024)
records.append(make_signal(
    "2024-02-10T08:00:00Z",
    "7GCihgDB8fe6KNjn2MYtkzZcRjQy3t9GHdC8uHYmW2hr",
    0.000100, 150000,
    ACCEPTED_WALLETS_TRAIN,
    [
        obs("2024-02-10T08:05:00Z", 0.000095),  # -5% -> SL
    ],
))

# 3. Train - Trailing stop (Mar 20, 2024)
# High at 0.000110, then drop to 0.000105 (> 4% drop from high)
records.append(make_signal(
    "2024-03-20T14:00:00Z",
    "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263",
    0.000100, 150000,
    ACCEPTED_WALLETS_TRAIN,
    [
        obs("2024-03-20T14:05:00Z", 0.000110),  # new high-water
        obs("2024-03-20T14:10:00Z", 0.000105),  # 4.5% drop -> trailing
    ],
))

# 4. Train - Time limit (Apr 15, 2024)
# Flat price path spanning 240 min with no trigger -> TimeLimit.
# Observations every 5 minutes from 10:05 through 14:00 (240 min).
def fmt_minutes(base_date, total_minutes):
    """Format base_date + total_minutes as a UTC timestamp string."""
    h, m = divmod(total_minutes, 60)
    return f"{base_date}T{10 + h:02d}:{m:02d}:00Z"

flat_obs = [
    obs(fmt_minutes("2024-04-15", m), 0.000100)
    for m in range(5, 245, 5)
]
records.append(make_signal(
    "2024-04-15T10:00:00Z",
    "7GCihgDB8fe6KNjn2MYtkzZcRjQy3t9GHdC8uHYmW2hr",
    0.000100, 150000,
    ACCEPTED_WALLETS_TRAIN,
    flat_obs,
))

# 5. Train - Censored (May 10, 2024)
# Insufficient data: 10 min of history < 240 min, no trigger
records.append(make_signal(
    "2024-05-10T09:00:00Z",
    "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263",
    0.000100, 150000,
    ACCEPTED_WALLETS_TRAIN,
    [
        obs("2024-05-10T09:05:00Z", 0.000100),
        obs("2024-05-10T09:10:00Z", 0.000100),
    ],
))

# 6. Train - Liquidity exit (May 20, 2024)
records.append(make_signal(
    "2024-05-20T14:00:00Z",
    "7GCihgDB8fe6KNjn2MYtkzZcRjQy3t9GHdC8uHYmW2hr",
    0.000100, 150000,
    ACCEPTED_WALLETS_TRAIN,
    [
        obs("2024-05-20T14:05:00Z", 0.000100, liquidity=40000),  # < 50000 -> liquidity exit
    ],
))

# 7. Train - "Ambiguous" (May 25, 2024)
# A price path that WOULD be ambiguous in OHLC terms: the price jumps
# from below SL to above TP between two consecutive observations.
# The single-point observation model means the engine resolves this as
# SL (the first observation below SL triggers the stop). The signal
# itself demonstrates the ambiguous-interval concept.
records.append(make_signal(
    "2024-05-25T11:00:00Z",
    "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263",
    0.000100, 150000,
    ACCEPTED_WALLETS_TRAIN,
    [
        obs("2024-05-25T11:05:00Z", 0.000094),  # < SL -> exit here
        obs("2024-05-25T11:10:00Z", 0.000115),  # would be ambiguous with obs 0
    ],
))

# 8. Train - Trailing stop (May 30, 2024)
records.append(make_signal(
    "2024-05-30T11:00:00Z",
    "7GCihgDB8fe6KNjn2MYtkzZcRjQy3t9GHdC8uHYmW2hr",
    0.000100, 150000,
    ACCEPTED_WALLETS_TRAIN,
    [
        obs("2024-05-30T11:05:00Z", 0.000110),  # high
        obs("2024-05-30T11:10:00Z", 0.000105),  # trailing
    ],
))

# ---------------------------------------------------------------------------
# VALIDATION split (2024-06-01 .. 2024-09-01)
# ---------------------------------------------------------------------------

# 8. Validation - TP (Jun 15, 2024)
records.append(make_signal(
    "2024-06-15T10:00:00Z",
    "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263",
    0.000100, 150000,
    ACCEPTED_WALLETS_VAL,
    [
        obs("2024-06-15T10:05:00Z", 0.000112),  # TP
    ],
))

# 9. Validation - SL (Jul 20, 2024)
records.append(make_signal(
    "2024-07-20T14:00:00Z",
    "7GCihgDB8fe6KNjn2MYtkzZcRjQy3t9GHdC8uHYmW2hr",
    0.000100, 150000,
    ACCEPTED_WALLETS_VAL,
    [
        obs("2024-07-20T14:05:00Z", 0.000095),  # SL
    ],
))

# 10. Validation - Trailing stop (Aug 10, 2024)
records.append(make_signal(
    "2024-08-10T12:00:00Z",
    "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263",
    0.000100, 150000,
    ACCEPTED_WALLETS_VAL,
    [
        obs("2024-08-10T12:05:00Z", 0.000110),
        obs("2024-08-10T12:10:00Z", 0.000105),
    ],
))

# 11. Validation - Liquidity exit (Aug 25, 2024)
records.append(make_signal(
    "2024-08-25T16:00:00Z",
    "7GCihgDB8fe6KNjn2MYtkzZcRjQy3t9GHdC8uHYmW2hr",
    0.000100, 150000,
    ACCEPTED_WALLETS_VAL,
    [
        obs("2024-08-25T16:05:00Z", 0.000100, liquidity=40000),
    ],
))

# ---------------------------------------------------------------------------
# OOS split (>= 2024-09-01)
# ---------------------------------------------------------------------------

# 12. OOS - TP (Sep 10, 2024) - positive
records.append(make_signal(
    "2024-09-10T09:00:00Z",
    "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263",
    0.000100, 150000,
    ACCEPTED_WALLETS_OOS_POS,
    [
        obs("2024-09-10T09:05:00Z", 0.000112),  # TP
    ],
))

# 13. OOS - TP (Sep 25, 2024) - positive
records.append(make_signal(
    "2024-09-25T11:00:00Z",
    "7GCihgDB8fe6KNjn2MYtkzZcRjQy3t9GHdC8uHYmW2hr",
    0.000100, 150000,
    ACCEPTED_WALLETS_OOS_POS,
    [
        obs("2024-09-25T11:05:00Z", 0.000112),  # TP
    ],
))

# 14. OOS - TP (Oct 10, 2024) - positive
records.append(make_signal(
    "2024-10-10T13:00:00Z",
    "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263",
    0.000100, 150000,
    ACCEPTED_WALLETS_OOS_POS,
    [
        obs("2024-10-10T13:05:00Z", 0.000112),  # TP
    ],
))

# 15. OOS - SL (Oct 25, 2024) - negative
records.append(make_signal(
    "2024-10-25T15:00:00Z",
    "7GCihgDB8fe6KNjn2MYtkzZcRjQy3t9GHdC8uHYmW2hr",
    0.000100, 150000,
    ACCEPTED_WALLETS_OOS_NEG,
    [
        obs("2024-10-25T15:05:00Z", 0.000095),  # SL
    ],
))

# 16. OOS - SL (Nov 10, 2024) - negative
records.append(make_signal(
    "2024-11-10T10:00:00Z",
    "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263",
    0.000100, 150000,
    ACCEPTED_WALLETS_OOS_NEG,
    [
        obs("2024-11-10T10:05:00Z", 0.000095),  # SL
    ],
))

# 17. OOS - TP (Nov 25, 2024) - positive
records.append(make_signal(
    "2024-11-25T14:00:00Z",
    "7GCihgDB8fe6KNjn2MYtkzZcRjQy3t9GHdC8uHYmW2hr",
    0.000100, 150000,
    ACCEPTED_WALLETS_OOS_POS,
    [
        obs("2024-11-25T14:05:00Z", 0.000112),  # TP
    ],
))

# 18. OOS - SL (Dec 5, 2024) - negative
records.append(make_signal(
    "2024-12-05T12:00:00Z",
    "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263",
    0.000100, 150000,
    ACCEPTED_WALLETS_OOS_NEG,
    [
        obs("2024-12-05T12:05:00Z", 0.000095),  # SL
    ],
))

# ---------------------------------------------------------------------------
# Rejected signal (strategy-level rejection, in OOS)
# Wallet score below min_wallet_score (60) -> wallet evidence rejection
# ---------------------------------------------------------------------------
records.append(make_signal(
    "2024-12-10T09:00:00Z",
    "7GCihgDB8fe6KNjn2MYtkzZcRjQy3t9GHdC8uHYmW2hr",
    0.000100, 150000,
    [
        wallet("0xBAD1", 50, "2024-12-10T08:50:00Z"),
        wallet("0xBAD2", 55, "2024-12-10T08:55:00Z"),
    ],
    [
        obs("2024-12-10T09:05:00Z", 0.000112),
    ],
))

# ---------------------------------------------------------------------------
# Future-dated invalid data (load-time PIT violation, in OOS)
# market.observed_at is AFTER signal_timestamp -> rejected at load time
# This is a PIT violation caught by the loader, not a strategy rejection.
# ---------------------------------------------------------------------------
records.append(make_signal(
    "2024-12-15T12:00:00Z",
    "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263",
    0.000100, 150000,
    ACCEPTED_WALLETS_OOS_POS,
    [
        obs("2024-12-15T12:05:00Z", 0.000112),
    ],
    market_observed_at="2024-12-15T13:00:00Z",  # AFTER signal_timestamp!
))

# Write the file. The SYNTHETIC / TEST DATA marker lives in
# data/README.SYNTHETIC so the JSONL itself is purely machine-readable.
with open(sys.argv[1], "w") as f:
    for rec in records:
        f.write(json.dumps(rec) + "\n")

print(f"Wrote {len(records)} records to {sys.argv[1]}")
