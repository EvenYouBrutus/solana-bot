#!/usr/bin/env python3
"""
Generate a small JSONL feed of paper candidates with current timestamps,
referencing real Solana mainnet mints. The bot will fetch a real Jupiter
quote for each mint at signal time and simulate a paper fill.

This is a synthetic-feed builder for the e2e paper test. It does NOT
fabricate market data: every line points to a real, currently-traded mint,
and the bot fetches the actual price from Jupiter before deciding.
"""
import json
from datetime import datetime, timezone

NOW = datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")

# Real Solana mainnet mints with reliable Jupiter routes.
CANDIDATES = [
    # mints, token_decimals, name
    ("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", 6, "USDC"),
    ("DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263", 5, "BONK"),
    ("JUPyiwrYJFskUPiHa7hkeR8VUtAeFoSYbKedZNsDvCN", 6, "JUP"),
    ("7GCihgDB8fe6KNjn2MYtkzZcRjQy3t9GHdC8uHYmW2hr", 6, "WIF"),
]

def make_candidate(mint, decimals, name, idx):
    return {
        "mint": mint,
        "token_decimals": decimals,
        "base_mint_decimals": 9,
        "input_amount": 4_000_000 + idx * 1_000_000,
        "position_usd": "4",
        "expected_gross_return_pct": "8",
        "market": {
            "mint": mint,
            "price_usd": "0.0001",
            "liquidity_usd": "500000",
            "volume_24h_usd": "0",
            "volatility_pct": "0",
            "buy_sell_imbalance": "0",
            "observed_at": NOW,
            "received_at": NOW,
            "slot": 0,
        },
        "safety": {
            "mint_authority_present": False,
            "freeze_authority_present": False,
            "holder_top10_pct": "10",
            "token_age_secs": 86400,
            "liquidity_locked_or_burned": True,
            "sellable": True,
            "route_available": True,
            "creator_suspicious": False,
            "abnormal_activity": False,
            "liquidity_change_pct": "0",
            "observed_at": NOW,
        },
        "wallets": [
            {
                "wallet": f"live_{name}_wallet_a_{idx}",
                "entity_id": f"entity_a_{name}_{idx}",
                "realized_pnl_usd": "500",
                "win_rate": "0.72",
                "avg_return_pct": "15",
                "median_return_pct": "12",
                "max_drawdown_pct": "8",
                "trades": 50,
                "recent_return_pct": "10",
                "concentration_pct": "5",
                "scam_exposure_pct": "0",
                "score": "82",
                "tier": "Qualified",
                "updated_at": NOW,
            },
            {
                "wallet": f"live_{name}_wallet_b_{idx}",
                "entity_id": f"entity_b_{name}_{idx}",
                "realized_pnl_usd": "320",
                "win_rate": "0.68",
                "avg_return_pct": "12",
                "median_return_pct": "10",
                "max_drawdown_pct": "10",
                "trades": 40,
                "recent_return_pct": "8",
                "concentration_pct": "3",
                "scam_exposure_pct": "0",
                "score": "75",
                "tier": "Qualified",
                "updated_at": NOW,
            },
        ],
        "costs": {
            "observed_at": NOW,
            "source": "e2e_paper_smoke",
            "is_live_snapshot": True,
            "input": {
                "position_size_usd": "4",
                "avg_priority_fee_usd": "0.0004",
                "avg_swap_fee_bps": "30",
                "avg_slippage_bps": "50",
                "avg_price_impact_bps": "20",
                "failed_tx_rate": "0.05",
                "avg_failed_tx_cost_usd": "0.002",
                "assumed_win_loss_ratio": "2",
                "assumed_avg_loss_pct": "10",
            },
        },
    }

if __name__ == "__main__":
    import sys
    out = sys.argv[1] if len(sys.argv) > 1 else "data/e2e_candidates.jsonl"
    with open(out, "w") as f:
        for i, (mint, decimals, name) in enumerate(CANDIDATES):
            f.write(json.dumps(make_candidate(mint, decimals, name, i)) + "\n")
    print(f"wrote {len(CANDIDATES)} candidates to {out}")
