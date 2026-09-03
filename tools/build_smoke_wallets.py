#!/usr/bin/env python3
"""
build_smoke_wallets.py – Build smart_wallets_smoke.txt from smart_wallets.txt.

Replicates the Rust swap parser semantics from swap_parser.rs EXACTLY.
Counts completed trades using FIFO lot tracking identical to wallet_monitor.rs.
Selects only wallets that satisfy the production Qualified requirement:
    completed_trades >= 25

Outputs:
    smart_wallets_smoke.txt   – one wallet per line (qualified only)
    smoke_wallets_report.csv  – diagnostic report for every wallet

Usage:
    python tools/build_smoke_wallets.py [--input FILE] [--output FILE] [--rpc URL]
"""

import argparse
import csv
import json
import os
import sys
import time
import logging
from collections import defaultdict, deque
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Deque, Dict, List, Optional, Tuple

try:
    import aiohttp
    import asyncio
except ImportError:
    print("ERROR: This script requires 'aiohttp'.  Install with:")
    print("  pip install aiohttp")
    sys.exit(1)

# ---------------------------------------------------------------------------
# Constants – must match swap_parser.rs exactly
# ---------------------------------------------------------------------------

WSOL_MINT = "So11111111111111111111111111111111111111112"

DEX_PROGRAMS = {
    "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4": "jupiter_v6",
    "JUP4Fb2cqiRUcaTHdrPC8h2gNsA2ETXiPDD33WcGuJB": "jupiter_v4",
    "JUP3jqKShLTC4TXbKQ9sRMSMgGKHGSuR3wHkJz2Bjqp": "jupiter_v3",
    "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8": "raydium_amm",
    "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK": "raydium_clmm",
    "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C": "raydium_cpmm",
    "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc": "orca_whirlpool",
    "PhoeNiXZ8ByJGLkxNfZRnkUfjvmuYqLR89jjFHGqdXY": "phoenix",
    "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSK9PK88t2jzdc": "meteora_dlmm",
    "Eo7WjKq67rjJQSZxS6z3YkapzS3LLjgeF31Rr9oFk7JE": "meteora_dyn_amm",
    "SSwapUtyTFBdGF1rCvjz4wp4Y8tE1dPi54Fo8SPauX6": "saber",
    "SwaPpA9LAaLfeLi3a68M4DjnLqgtticKg6CnyNwgAC8": "saber",
}

DUST_LAMPORTS = 1_000
MIN_SWAP_LAMPORTS = 1_000

# RPC settings
RPC_RATE_LIMIT_MS = 200          # ms between RPC calls (public RPC is slow)
MAX_CONCURRENCY = 2              # parallel wallet fetches (public RPC is fragile)
MAX_SIGNATURES_PER_WALLET = 1000 # paginated signature fetch cap
RETRY_ATTEMPTS = 5
RETRY_BASE_BACKOFF = 1.0        # seconds, doubles each retry
COMPLETED_TRADES_THRESHOLD = 25  # production Qualified requirement

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [%(levelname)s] %(message)s",
    datefmt="%H:%M:%S",
)
log = logging.getLogger("smoke")


# ---------------------------------------------------------------------------
# RPC helpers
# ---------------------------------------------------------------------------

async def rpc_call(
    session: aiohttp.ClientSession,
    url: str,
    method: str,
    params: list,
    semaphore: asyncio.Semaphore,
) -> Any:
    """Make a single JSON-RPC call with retry and rate limiting."""
    payload = {"jsonrpc": "2.0", "id": 1, "method": method, "params": params}
    for attempt in range(RETRY_ATTEMPTS):
        async with semaphore:
            await asyncio.sleep(RPC_RATE_LIMIT_MS / 1000.0)
            try:
                async with session.post(url, json=payload, timeout=aiohttp.ClientTimeout(total=15)) as resp:
                    if resp.status == 429:
                        backoff = RETRY_BASE_BACKOFF * (2 ** attempt)
                        retry_after = resp.headers.get("Retry-After")
                        wait = float(retry_after) if retry_after else backoff
                        log.warning("Rate limited (429), backing off %.1fs", wait)
                        await asyncio.sleep(wait)
                        continue
                    if resp.status != 200:
                        text = await resp.text()
                        raise RuntimeError(f"HTTP {resp.status}: {text[:200]}")
                    data = await resp.json()
                    if "error" in data:
                        err = data["error"]
                        code = err.get("code", -1)
                        # -32600..-32603 are server errors; retry
                        if code in (-32600, -32601, -32602, -32603, -32002, -32005, -32006):
                            backoff = RETRY_BASE_BACKOFF * (2 ** attempt)
                            log.warning("RPC error %s (code %d), retrying in %.1fs", err.get("message"), code, backoff)
                            await asyncio.sleep(backoff)
                            continue
                        raise RuntimeError(f"RPC error: {err}")
                    return data.get("result")
            except (aiohttp.ClientError, asyncio.TimeoutError) as e:
                backoff = RETRY_BASE_BACKOFF * (2 ** attempt)
                log.warning("Network error: %s, retrying in %.1fs", e, backoff)
                await asyncio.sleep(backoff)
    raise RuntimeError(f"RPC call failed after {RETRY_ATTEMPTS} attempts: {method}")


async def get_signatures(
    session: aiohttp.ClientSession,
    url: str,
    wallet: str,
    semaphore: asyncio.Semaphore,
    max_sigs: int = 200,
) -> List[dict]:
    """Fetch all signatures for a wallet, paginated up to max_sigs."""
    all_sigs = []
    before = None
    while len(all_sigs) < max_sigs:
        limit = min(1000, max_sigs - len(all_sigs))
        params: list = [wallet, {"limit": limit, "commitment": "finalized"}]
        if before:
            params[1]["before"] = before
        result = await rpc_call(session, url, "getSignaturesForAddress", params, semaphore)
        if not result:
            break
        all_sigs.extend(result)
        if len(result) < limit:
            break
        before = result[-1]["signature"]
    return all_sigs


async def get_transaction(
    session: aiohttp.ClientSession,
    url: str,
    sig: str,
    semaphore: asyncio.Semaphore,
) -> Optional[dict]:
    """Fetch a single confirmed transaction."""
    result = await rpc_call(
        session,
        url,
        "getTransaction",
        [sig, {"encoding": "json", "maxSupportedTransactionVersion": 0, "commitment": "finalized"}],
        semaphore,
    )
    return result


# ---------------------------------------------------------------------------
# Swap parser – replicates swap_parser.rs semantics EXACTLY
# ---------------------------------------------------------------------------

def extract_pubkey(key: Any) -> Optional[str]:
    if isinstance(key, str):
        return key
    if isinstance(key, dict):
        return key.get("pubkey")
    return None


def build_all_account_keys(tx: dict) -> Optional[List[str]]:
    """Replicate build_all_account_keys from swap_parser.rs."""
    msg = tx.get("transaction", {}).get("message", {})
    static_keys_raw = msg.get("accountKeys", [])
    keys = []
    for k in static_keys_raw:
        pk = extract_pubkey(k)
        if pk:
            keys.append(pk)

    loaded = tx.get("meta", {}).get("loadedAddresses")
    if loaded and isinstance(loaded, dict):
        for k in loaded.get("writable", []):
            if isinstance(k, str):
                keys.append(k)
            elif isinstance(k, dict) and "pubkey" in k:
                keys.append(k["pubkey"])
        for k in loaded.get("readonly", []):
            if isinstance(k, str):
                keys.append(k)
            elif isinstance(k, dict) and "pubkey" in k:
                keys.append(k["pubkey"])
    elif msg.get("addressTableLookups"):
        pass  # Cannot resolve lookup tables; skip silently

    return keys


def parse_token_balances_for_owner(balances: Any, wallet: str) -> Dict[str, Tuple[int, int]]:
    """Replicate parse_token_balances_for_owner from swap_parser.rs."""
    result: Dict[str, Tuple[int, int]] = {}
    if not isinstance(balances, list):
        return result
    for entry in balances:
        if entry.get("owner") != wallet:
            continue
        mint = entry.get("mint")
        if not mint:
            continue
        ui_amount = entry.get("uiTokenAmount", {})
        amount_str = ui_amount.get("amount", "0")
        try:
            amount = int(amount_str)
        except (ValueError, TypeError):
            amount = 0
        decimals = int(ui_amount.get("decimals", 0))
        if mint in result:
            prev_amount, prev_dec = result[mint]
            result[mint] = (prev_amount + amount, prev_dec)
        else:
            result[mint] = (amount, decimals)
    return result


def detect_dex(tx: dict, all_keys: List[str]) -> str:
    """Replicate detect_dex from swap_parser.rs."""
    msg = tx.get("transaction", {}).get("message", {})
    top_ixs = msg.get("instructions", [])
    inner_blocks = tx.get("meta", {}).get("innerInstructions", [])

    def check(program_idx: int) -> str:
        if program_idx < len(all_keys):
            key = all_keys[program_idx]
            name = DEX_PROGRAMS.get(key, "")
            return name
        return ""

    for ix in top_ixs:
        pid = ix.get("programIdIndex")
        if pid is not None:
            d = check(int(pid))
            if d and d != "ignore":
                return d

    for block in inner_blocks:
        for ix in block.get("instructions", []):
            pid = ix.get("programIdIndex")
            if pid is not None:
                d = check(int(pid))
                if d and d != "ignore":
                    return d

    return "unknown"


@dataclass
class ParsedSwap:
    wallet: str
    input_mint: str
    output_mint: str
    input_amount: int
    output_amount: int
    input_decimals: int
    output_decimals: int
    direction: str  # "Buy" or "Sell"
    fee_lamports: int
    dex: str
    slot: int
    block_time: int
    signature: str


def parse_swap_from_transaction(tx: dict, wallet: str) -> Optional[ParsedSwap]:
    """Replicate parse_swap_from_transaction from swap_parser.rs EXACTLY."""
    meta = tx.get("meta")
    if not meta:
        return None
    if meta.get("err") is not None:
        return None

    sigs = tx.get("transaction", {}).get("signatures", [])
    signature = sigs[0] if sigs else ""
    slot = tx.get("slot", 0) or 0
    block_time = tx.get("blockTime", 0) or 0
    fee = meta.get("fee", 0) or 0

    all_account_keys = build_all_account_keys(tx)
    if all_account_keys is None:
        return None

    try:
        wallet_idx = all_account_keys.index(wallet)
    except ValueError:
        return None

    pre_balances = meta.get("preBalances", [])
    post_balances = meta.get("postBalances", [])
    if len(pre_balances) != len(post_balances):
        return None
    if wallet_idx >= len(pre_balances) or wallet_idx >= len(post_balances):
        return None

    pre_sol = pre_balances[wallet_idx]
    post_sol = post_balances[wallet_idx]

    pre_tokens = parse_token_balances_for_owner(meta.get("preTokenBalances", []), wallet)
    post_tokens = parse_token_balances_for_owner(meta.get("postTokenBalances", []), wallet)

    all_mints_set = set()
    all_mints = []
    for m in list(pre_tokens.keys()) + list(post_tokens.keys()):
        if m not in all_mints_set:
            all_mints_set.add(m)
            all_mints.append(m)

    wsol_delta = 0
    gained: List[Tuple[str, int, int]] = []  # (mint, amount, decimals)
    lost: List[Tuple[str, int, int]] = []

    for mint in all_mints:
        pre_amt = pre_tokens.get(mint, (0, 0))[0]
        post_amt = post_tokens.get(mint, (0, 0))[0]
        decimals = post_tokens.get(mint, (0, 0))[1] or pre_tokens.get(mint, (0, 0))[1] or 0

        delta = post_amt - pre_amt
        if mint == WSOL_MINT:
            wsol_delta = delta
        elif delta > 0:
            gained.append((mint, delta, decimals))
        elif delta < 0:
            lost.append((mint, -delta, decimals))

    native_sol_change = post_sol - pre_sol
    total_sol_spent = -native_sol_change - wsol_delta - fee

    dex = detect_dex(tx, all_account_keys)
    if dex == "unknown":
        return None

    # CASE 1: BUY (SOL/WSOL -> token)
    if total_sol_spent > MIN_SWAP_LAMPORTS and gained:
        primary = None
        # Filter dust, then take max
        non_dust = [(m, a, d) for m, a, d in gained if a >= DUST_LAMPORTS]
        if non_dust:
            primary = max(non_dust, key=lambda x: x[1])
        else:
            primary = max(gained, key=lambda x: x[1])
        if primary:
            mint, amount, decimals = primary
            return ParsedSwap(
                wallet=wallet,
                input_mint=WSOL_MINT,
                output_mint=mint,
                input_amount=total_sol_spent,
                output_amount=amount,
                input_decimals=9,
                output_decimals=decimals,
                direction="Buy",
                fee_lamports=fee,
                dex=dex,
                slot=slot,
                block_time=block_time,
                signature=signature,
            )

    # CASE 2: SELL (token -> SOL/WSOL)
    if total_sol_spent < -MIN_SWAP_LAMPORTS and lost:
        primary = None
        non_dust = [(m, a, d) for m, a, d in lost if a >= DUST_LAMPORTS]
        if non_dust:
            primary = max(non_dust, key=lambda x: x[1])
        else:
            primary = max(lost, key=lambda x: x[1])
        if primary:
            mint, amount, decimals = primary
            return ParsedSwap(
                wallet=wallet,
                input_mint=mint,
                output_mint=WSOL_MINT,
                input_amount=amount,
                output_amount=-total_sol_spent,
                input_decimals=decimals,
                output_decimals=9,
                direction="Sell",
                fee_lamports=fee,
                dex=dex,
                slot=slot,
                block_time=block_time,
                signature=signature,
            )

    # CASE 3: Token-to-token swap (negligible SOL change)
    if lost and gained and abs(total_sol_spent) < MIN_SWAP_LAMPORTS:
        primary_lost = None
        non_dust_lost = [(m, a, d) for m, a, d in lost if a >= DUST_LAMPORTS]
        if non_dust_lost:
            primary_lost = max(non_dust_lost, key=lambda x: x[1])
        else:
            primary_lost = max(lost, key=lambda x: x[1])

        primary_gained = None
        non_dust_gained = [(m, a, d) for m, a, d in gained if a >= DUST_LAMPORTS]
        if non_dust_gained:
            primary_gained = max(non_dust_gained, key=lambda x: x[1])
        else:
            primary_gained = max(gained, key=lambda x: x[1])

        if primary_lost and primary_gained:
            input_mint, input_amount, input_dec = primary_lost
            output_mint, output_amount, output_dec = primary_gained
            if input_mint == WSOL_MINT:
                direction = "Buy"
            elif output_mint == WSOL_MINT:
                direction = "Sell"
            else:
                direction = "Buy"
            return ParsedSwap(
                wallet=wallet,
                input_mint=input_mint,
                output_mint=output_mint,
                input_amount=input_amount,
                output_amount=output_amount,
                input_decimals=input_dec,
                output_decimals=output_dec,
                direction=direction,
                fee_lamports=fee,
                dex=dex,
                slot=slot,
                block_time=block_time,
                signature=signature,
            )

    return None


# ---------------------------------------------------------------------------
# FIFO lot tracker – replicates wallet_monitor.rs WalletAccumulator
# ---------------------------------------------------------------------------

@dataclass
class OpenPosition:
    sol_spent: float  # SOL spent (float for simplicity; precision sufficient for screening)
    tokens_received: float
    timestamp: int  # block_time


@dataclass
class CompletedTrade:
    return_pct: float
    pnl_sol: float
    entry_time: int
    exit_time: int


@dataclass
class WalletAccumulator:
    open_positions: Dict[str, Deque[OpenPosition]] = field(default_factory=lambda: defaultdict(deque))
    completed_trades: List[CompletedTrade] = field(default_factory=list)
    buys: int = 0
    sells: int = 0
    dex_activity: Dict[str, int] = field(default_factory=lambda: defaultdict(int))
    last_activity_ts: Optional[int] = None
    first_activity_ts: Optional[int] = None

    def record_observation(self, swap: ParsedSwap):
        if self.last_activity_ts is None or swap.block_time > self.last_activity_ts:
            self.last_activity_ts = swap.block_time
        if self.first_activity_ts is None or swap.block_time < self.first_activity_ts:
            self.first_activity_ts = swap.block_time

        self.dex_activity[swap.dex] += 1
        sol_amount = float(swap.input_amount) / 1e9 if swap.direction == "Buy" else float(swap.output_amount) / 1e9
        tokens = float(swap.output_amount) if swap.direction == "Buy" else float(swap.input_amount)
        mint = swap.output_mint if swap.direction == "Buy" else swap.input_mint
        ts = swap.block_time

        if swap.direction == "Buy":
            self.buys += 1
            self.open_positions[mint].append(OpenPosition(
                sol_spent=sol_amount,
                tokens_received=tokens,
                timestamp=ts,
            ))
        elif swap.direction == "Sell":
            self.sells += 1
            self._record_sell(mint, tokens, sol_amount, ts)

    def _record_sell(self, mint: str, tokens_sold: float, sol_received: float, sell_time: int):
        queue = self.open_positions.get(mint)
        if not queue:
            return

        remaining_to_sell = tokens_sold
        total_proceeds = sol_received
        consumed = 0

        # First pass: pop fully-consumed lots
        while queue and remaining_to_sell > 0:
            front = queue[0]
            if front.tokens_received <= remaining_to_sell:
                lot = queue.popleft()
                share = lot.tokens_received / tokens_sold if tokens_sold > 0 else 0
                lot_proceeds = total_proceeds * share
                return_pct = ((lot_proceeds - lot.sol_spent) / lot.sol_spent * 100) if lot.sol_spent > 0 else 0
                pnl = lot_proceeds - lot.sol_spent
                self.completed_trades.append(CompletedTrade(
                    return_pct=return_pct,
                    pnl_sol=pnl,
                    entry_time=lot.timestamp,
                    exit_time=sell_time,
                ))
                remaining_to_sell -= lot.tokens_received
                consumed += 1
            else:
                break

        # Partial lot
        if remaining_to_sell > 0 and queue:
            front = queue[0]
            share = remaining_to_sell / tokens_sold if tokens_sold > 0 else 0
            lot_proceeds = total_proceeds * share
            return_pct = ((lot_proceeds - front.sol_spent) / front.sol_spent * 100) if front.sol_spent > 0 else 0
            pnl = lot_proceeds - front.sol_spent
            self.completed_trades.append(CompletedTrade(
                return_pct=return_pct,
                pnl_sol=pnl,
                entry_time=front.timestamp,
                exit_time=sell_time,
            ))
            still_remaining = front.tokens_received - remaining_to_sell
            if still_remaining <= 0:
                queue.popleft()
            else:
                front.tokens_received = still_remaining
            consumed += 1


# ---------------------------------------------------------------------------
# Wallet processing
# ---------------------------------------------------------------------------

@dataclass
class WalletReport:
    wallet: str
    signatures_total: int = 0
    signatures_fetched: int = 0
    recognized_swaps: int = 0
    buys: int = 0
    sells: int = 0
    completed_trades: int = 0
    last_activity: Optional[str] = None
    qualified: bool = False
    error: Optional[str] = None
    dex_breakdown: Dict[str, int] = field(default_factory=dict)
    skipped_signatures: int = 0


async def process_wallet(
    session: aiohttp.ClientSession,
    url: str,
    wallet: str,
    semaphore: asyncio.Semaphore,
    progress: dict,
    max_sigs: int = 200,
) -> WalletReport:
    """Fetch, parse, and score a single wallet."""
    report = WalletReport(wallet=wallet)

    # Step 1: Get signatures
    try:
        sigs = await get_signatures(session, url, wallet, semaphore, max_sigs)
    except Exception as e:
        report.error = f"getSignatures failed: {e}"
        log.error("[%s] %s", wallet[:12], report.error)
        progress["done"] = progress.get("done", 0) + 1
        return report

    report.signatures_total = len(sigs)
    report.signatures_fetched = len(sigs)

    if not sigs:
        report.error = "no signatures"
        log.info("[%s] No signatures found", wallet[:12])
        progress["done"] = progress.get("done", 0) + 1
        return report

    # Step 2: Filter failed txs and already-seen sigs
    valid_sigs = []
    seen = set()
    for s in sigs:
        if s.get("err") is not None:
            report.skipped_signatures += 1
            continue
        sig_str = s["signature"]
        if sig_str in seen:
            continue
        seen.add(sig_str)
        valid_sigs.append(s)

    # Step 3: Fetch and parse transactions
    acc = WalletAccumulator()
    parse_errors = 0

    # Fetch sequentially with small batches to respect rate limits on public RPC
    batch_size = 3
    for i in range(0, len(valid_sigs), batch_size):
        batch = valid_sigs[i:i + batch_size]
        for s in batch:
            try:
                result = await get_transaction(session, url, s["signature"], semaphore)
                if result is None:
                    continue
                swap = parse_swap_from_transaction(result, wallet)
                if swap is None:
                    continue
                acc.record_observation(swap)
                report.recognized_swaps += 1
            except Exception as e:
                parse_errors += 1
                log.debug("[%s] Failed to parse %s: %s", wallet[:12], s["signature"][:16], e)
        # Extra delay between batches for public RPC
        if i + batch_size < len(valid_sigs):
            await asyncio.sleep(0.3)

    # Step 4: Compile report
    report.buys = acc.buys
    report.sells = acc.sells
    report.completed_trades = len(acc.completed_trades)
    report.dex_breakdown = dict(acc.dex_activity)

    if acc.last_activity_ts:
        report.last_activity = datetime.fromtimestamp(
            acc.last_activity_ts, tz=timezone.utc
        ).strftime("%Y-%m-%dT%H:%M:%SZ")

    report.qualified = report.completed_trades >= COMPLETED_TRADES_THRESHOLD

    log.info(
        "[%s] sigs=%d swaps=%d buys=%d sells=%d completed=%d dex=%s%s",
        wallet[:12],
        report.signatures_fetched,
        report.recognized_swaps,
        report.buys,
        report.sells,
        report.completed_trades,
        report.dex_breakdown,
        f" parse_errors={parse_errors}" if parse_errors else "",
    )

    progress["done"] = progress.get("done", 0) + 1
    return report


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

async def main():
    parser = argparse.ArgumentParser(description="Build smart_wallets_smoke.txt from smart_wallets.txt")
    parser.add_argument("--input", default="smart_wallets.txt", help="Input wallet list (default: smart_wallets.txt)")
    parser.add_argument("--output", default="smart_wallets_smoke.txt", help="Output smoke file (default: smart_wallets_smoke.txt)")
    parser.add_argument("--report", default="smoke_wallets_report.csv", help="Diagnostic CSV report (default: smoke_wallets_report.csv)")
    parser.add_argument("--rpc", default=None, help="Solana RPC URL (default: $SOLANA_RPC_URL or api.mainnet-beta.solana.com)")
    parser.add_argument("--max-wallets", type=int, default=0, help="Process only first N wallets (0 = all)")
    parser.add_argument("--max-sigs", type=int, default=200, help="Max signatures to fetch per wallet (default: 200)")
    parser.add_argument("--min-completed", type=int, default=COMPLETED_TRADES_THRESHOLD,
                        help=f"Minimum completed trades for Qualified (default: {COMPLETED_TRADES_THRESHOLD})")
    args = parser.parse_args()

    rpc_url = args.rpc or os.environ.get("SOLANA_RPC_URL") or "https://api.mainnet-beta.solana.com"
    log.info("RPC URL: %s", rpc_url)

    # Read wallets
    input_path = Path(args.input)
    if not input_path.exists():
        log.error("Input file not found: %s", input_path)
        sys.exit(1)

    wallets = []
    for line in input_path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        wallets.append(line)

    if not wallets:
        log.error("No wallets found in %s", input_path)
        sys.exit(1)

    log.info("Loaded %d wallets from %s", len(wallets), input_path)

    if args.max_wallets > 0:
        wallets = wallets[:args.max_wallets]
        log.info("Processing first %d wallets (max-wallets)", len(wallets))

    # Process wallets with bounded concurrency
    semaphore = asyncio.Semaphore(MAX_CONCURRENCY)
    progress = {"done": 0}

    connector = aiohttp.TCPConnector(limit=MAX_CONCURRENCY, force_close=True)
    async with aiohttp.ClientSession(connector=connector) as session:
        tasks = [process_wallet(session, rpc_url, w, semaphore, progress, args.max_sigs) for w in wallets]

        # Run with progress updates
        start = time.time()
        reports = []
        for coro in asyncio.as_completed(tasks):
            report = await coro
            reports.append(report)
            done = progress.get("done", 0)
            if done % 10 == 0 or done == len(wallets):
                elapsed = time.time() - start
                rate = done / elapsed if elapsed > 0 else 0
                log.info("Progress: %d/%d wallets (%.1f/s)", done, len(wallets), rate)

    # Sort by wallet address for deterministic output
    reports.sort(key=lambda r: r.wallet)

    # Separate qualified and non-qualified
    qualified = [r for r in reports if r.qualified]
    non_qualified = [r for r in reports if not r.qualified]

    log.info("=" * 60)
    log.info("RESULTS: %d qualified, %d non-qualified out of %d total", len(qualified), len(non_qualified), len(reports))
    log.info("=" * 60)

    if qualified:
        log.info("Qualified wallets:")
        for r in qualified:
            log.info("  %s  completed=%d  swaps=%d  buys=%d  sells=%d  last=%s",
                     r.wallet, r.completed_trades, r.recognized_swaps, r.buys, r.sells, r.last_activity)
    else:
        log.warning("No wallets met the Qualified threshold (completed_trades >= %d)", args.min_completed)
        log.info("Top candidates by completed_trades:")
        top = sorted(reports, key=lambda r: r.completed_trades, reverse=True)[:10]
        for r in top:
            log.info("  %s  completed=%d  swaps=%d  buys=%d  sells=%d",
                     r.wallet, r.completed_trades, r.recognized_swaps, r.buys, r.sells)

    # Write output smoke file
    output_path = Path(args.output)
    with open(output_path, "w") as f:
        f.write(f"# Generated by tools/build_smoke_wallets.py on {datetime.now(timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')}\n")
        f.write(f"# Source: {input_path} ({len(wallets)} wallets scanned)\n")
        f.write(f"# Qualified wallets: {len(qualified)} (completed_trades >= {args.min_completed})\n")
        f.write(f"# RPC: {rpc_url}\n")
        f.write("#\n")
        f.write("# Each address below satisfies the production Qualified requirement:\n")
        f.write("# completed_trades >= 25 (FIFO buy-sell pairs, same semantics as wallet_monitor.rs)\n")
        f.write("#\n")
        for r in qualified:
            f.write(f"{r.wallet}\n")
    log.info("Wrote %d qualified wallets to %s", len(qualified), output_path)

    # Write diagnostic CSV report
    report_path = Path(args.report)
    with open(report_path, "w", newline="") as f:
        writer = csv.writer(f)
        writer.writerow([
            "wallet", "signatures_total", "signatures_fetched", "recognized_swaps",
            "buys", "sells", "completed_trades", "last_activity", "qualified",
            "error", "dex_breakdown"
        ])
        for r in reports:
            writer.writerow([
                r.wallet, r.signatures_total, r.signatures_fetched, r.recognized_swaps,
                r.buys, r.sells, r.completed_trades, r.last_activity, r.qualified,
                r.error or "", json.dumps(r.dex_breakdown) if r.dex_breakdown else "",
            ])
    log.info("Wrote diagnostic report to %s", report_path)


if __name__ == "__main__":
    asyncio.run(main())
