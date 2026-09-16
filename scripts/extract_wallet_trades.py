#!/usr/bin/env python3
"""
Extract wallet trade histories from GeckoTerminal for backtesting.

Usage:
    python scripts/extract_wallet_trades.py --pool <POOL_ID> --output <FILE> [--pages <N>]

Output: JSONL file with one WalletTrade per line, compatible with the
        Rust pipeline's --wallet-trades-file parameter.
"""
import argparse
import json
import time
import sys
import urllib.request
import urllib.error

BASE_URL = "https://api.geckoterminal.com"

def fetch_trades(pool_id: str, page: int = 1) -> list:
    """Fetch trades from GeckoTerminal API."""
    url = f"{BASE_URL}/api/v2/networks/solana/pools/{pool_id}/trades?page={page}"
    req = urllib.request.Request(url, headers={"User-Agent": "solana-bot-backtest/1.0"})
    try:
        with urllib.request.urlopen(req, timeout=15) as resp:
            data = json.loads(resp.read())
            return data.get("data", [])
    except (urllib.error.URLError, urllib.error.HTTPError) as e:
        print(f"  Error fetching page {page}: {e}", file=sys.stderr)
        return []

def parse_trade(trade: dict, pool_id: str) -> dict:
    """Parse a GeckoTerminal trade into a WalletTrade-compatible dict."""
    attrs = trade.get("attributes", {})
    wallet = attrs.get("tx_from_address", "")
    kind = attrs.get("kind", "")
    block_ts = attrs.get("block_timestamp", "")
    tx_hash = attrs.get("tx_hash", "")
    from_amount = attrs.get("from_token_amount", "0")
    to_amount = attrs.get("to_token_amount", "0")
    from_addr = attrs.get("from_token_address", "")
    to_addr = attrs.get("to_token_address", "")
    volume_usd = attrs.get("volume_in_usd", "0")
    block_number = attrs.get("block_number", 0)

    if not wallet or not block_ts:
        return None

    # Determine side and mint based on the trade direction
    # For our purposes: if from_token is the target mint, it's a SELL
    # if to_token is the target mint, it's a BUY
    side = "Buy" if kind == "buy" else "Sell"
    mint = to_addr if kind == "buy" else from_addr
    notional = float(volume_usd) if volume_usd else 0.0

    return {
        "wallet": wallet,
        "mint": mint,
        "side": side,
        "notional_usd": str(notional),
        "block_time": block_ts,
        "signature": tx_hash,
        "slot": block_number,
    }

def main():
    parser = argparse.ArgumentParser(description="Extract wallet trades from GeckoTerminal")
    parser.add_argument("--pool", required=True, help="GeckoTerminal pool ID (without solana_ prefix)")
    parser.add_argument("--output", required=True, help="Output JSONL file")
    parser.add_argument("--pages", type=int, default=20, help="Number of pages to fetch (100 trades/page)")
    parser.add_argument("--delay", type=float, default=2.0, help="Seconds between API requests")
    args = parser.parse_args()

    all_trades = []
    seen_sigs = set()
    seen_wallets = set()

    print(f"Fetching trades from pool {args.pool}...")
    for page in range(1, args.pages + 1):
        trades = fetch_trades(args.pool, page)
        if not trades:
            print(f"  Page {page}: no more trades")
            break
        for t in trades:
            parsed = parse_trade(t, args.pool)
            if parsed and parsed["signature"] not in seen_sigs:
                seen_sigs.add(parsed["signature"])
                seen_wallets.add(parsed["wallet"])
                all_trades.append(parsed)
        print(f"  Page {page}: {len(trades)} trades, {len(all_trades)} unique, {len(seen_wallets)} wallets")
        if page < args.pages:
            time.sleep(args.delay)

    # Sort by block_time
    all_trades.sort(key=lambda t: t["block_time"])

    # Write output
    with open(args.output, "w") as f:
        for t in all_trades:
            f.write(json.dumps(t) + "\n")

    print(f"\nDone: {len(all_trades)} trades from {len(seen_wallets)} wallets")
    print(f"Output: {args.output}")

    # Print wallet summary
    wallet_counts = {}
    for t in all_trades:
        w = t["wallet"]
        wallet_counts[w] = wallet_counts.get(w, 0) + 1
    print("\nTop wallets by trade count:")
    for w, c in sorted(wallet_counts.items(), key=lambda x: -x[1])[:10]:
        print(f"  {w}: {c} trades")

if __name__ == "__main__":
    main()
