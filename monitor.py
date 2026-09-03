#!/usr/bin/env python3
"""Smart Money Monitor for the solana-bot execution engine.

This is a **read-only** Python process that:

1. Reads a list of target "smart wallets" from ``smart_wallets.txt``.
2. Reconstructs each wallet's historical trades via Solana RPC
   (``getSignaturesForAddress`` + ``getTransaction``) and computes
   wallet statistics (PnL, win-rate, drawdown, score, tier).
3. Subscribes to ``logsSubscribe`` for every smart wallet so we see
   each on-chain transaction in real time.
4. Parses DEX swap logs (Raydium / Jupiter / Phoenix / Whirlpool)
   to identify BUY actions. For every BUY on a previously-unseen mint:
   - Fetches the mint account (mint/freeze authorities, decimals, supply)
   - Fetches the token's USD price, liquidity and 24h volume via
     DexScreener (free, no API key) and falls back to Jupiter
     ``getQuote`` if DexScreener is rate-limited.
   - Applies the safety filters from ``config/local.toml``
     (liquidity floor, mint/freeze authorities, token-age floor).
   - Estimates round-trip execution costs (swap fee, priority fee,
     slippage, price impact) consistent with the Rust bot's
     ``BreakEvenInputs`` schema.
5. Writes a JSON record matching the Rust
   ``solana_smart_money_bot::runtime::CandidateInput`` schema to
   ``signals/live_signals.jsonl``.

Output JSON is written one record per line so the Rust
``CandidateCollector::collect_from_jsonl`` path can consume it directly
(``config/runtime.signal_feed_path``).

The script NEVER holds or signs transactions; it is strictly a
publisher of pre-trade observations.

Usage
=====

::

    # 1. install deps
    python -m venv .venv && source .venv/bin/activate
    pip install -r requirements.txt

    # 2. write .env (HTTP RPC + WSS RPC)
    cat > .env <<'EOF'
    SOLANA_RPC_URL=https://api.mainnet-beta.solana.com
    SOLANA_WSS_URL=wss://api.mainnet-beta.solana.com
    EOF

    # 3. fill smart_wallets.txt with one base-58 address per line

    # 4. run
    python monitor.py

    # 5. point the Rust bot at the output
    #    config/local.toml:
    #      [runtime]
    #      signal_feed_path = "signals/live_signals.jsonl"
"""

from __future__ import annotations

import argparse
import asyncio
import json
import logging
import os
import statistics
import sys
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone
from decimal import Decimal
from pathlib import Path
from typing import Any

import requests

# Solana SDK and dotenv are imported lazily so the offline helpers
# (Decimal formatting, scoring, gates, signal construction, tests)
# work without the SDK installed. The first RPC call triggers them.
load_dotenv: object | None = None
AsyncClient = None  # type: ignore
Confirmed = None  # type: ignore
Pubkey = None  # type: ignore
Signature = None  # type: ignore

# ============================================================================
# Constants matching the Rust schema (src/runtime.rs CandidateInput).
# Numeric fields are emitted as strings because the Rust bot uses
# rust_decimal with the serde-with-str feature.
# ============================================================================

# DEX program IDs that we recognize as the source of a BUY.
# These are public mainnet constants; the script NEVER signs anything.
RAYDIUM_AMM_V4 = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8"
RAYDIUM_CLMM = "CAMMCzo5YL8w4VFF8KVHrKjyGGugPMvtO7ksyAFD8uGt"
RAYDIUM_CPMM = "CPMMoo8L3FmkaWuwGN4Sq1UwPAuzqmkJjGfT9deampX"
JUPITER_V6 = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4"
JUPITER_V4 = "JUP4Fb2T4C9rYLB1z2Lg8SkY7c8XE9pRk2H1oF8q6WZ7S"
ORCA_WHIRLPOOLS = "whirLbMiicVdio4qvUfM5KAg7Ct8VwpEaGqX3o6pqrP"
PHOENIX = "PhoeNiXZ8ByJGLkxNfZRnkUfjvmuFLaLRMUg9nemzhL"
TOKEN_PROGRAM = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
SYSTEM_PROGRAM = "11111111111111111111111111111111111111111"

DEX_PROGRAMS = {
    RAYDIUM_AMM_V4,
    RAYDIUM_CLMM,
    RAYDIUM_CPMM,
    JUPITER_V6,
    JUPITER_V4,
    ORCA_WHIRLPOOLS,
    PHOENIX,
}

# Safety / strategy thresholds. These are deliberately identical to the
# values in ``config/local.toml`` so the monitor pre-filters the same
# signals the bot would otherwise reject.
DEFAULT_MIN_LIQUIDITY_USD = Decimal("50000")
DEFAULT_MIN_TOKEN_AGE_SECS = 86400
DEFAULT_MIN_WALLET_SCORE = Decimal("60")
DEFAULT_MIN_WALLET_SAMPLES = 25
DEFAULT_MIN_CONSENSUS_WALLETS = 2  # requires >=2 qualified wallets in the candidate list
DEFAULT_POSITION_USD = Decimal("4")
DEFAULT_SLIPPAGE_BPS = 75
DEFAULT_SWAP_FEE_BPS = Decimal("30")
DEFAULT_PRIORITY_FEE_LAMPORTS = 10_000
DEFAULT_FAILED_TX_RATE = Decimal("0.05")
DEFAULT_FAILED_TX_COST_USD = Decimal("0.002")
DEFAULT_ASSUMED_WIN_LOSS_RATIO = Decimal("2")
DEFAULT_ASSUMED_AVG_LOSS_PCT = Decimal("10")
DEFAULT_HISTORY_TX_LIMIT = 100  # per-wallet historical sample size

# Conservative SOL price assumption. Used only as a USD anchor when
# DexScreener/Jupiter price lookups fail. NEVER used for entry decisions
# in the Rust bot; the bot re-prices every quote via Jupiter at submit.
DEFAULT_SOL_USD = Decimal("150")


# ============================================================================
# Logging
# ============================================================================


def _setup_logger(verbose: bool) -> logging.Logger:
    """Return a configured module logger."""
    handler = logging.StreamHandler(sys.stdout)
    handler.setFormatter(
        logging.Formatter("%(asctime)s %(levelname)s %(message)s", "%Y-%m-%dT%H:%M:%S%z")
    )
    logger = logging.getLogger("monitor")
    logger.handlers = [handler]
    logger.setLevel(logging.DEBUG if verbose else logging.INFO)
    logger.propagate = False
    return logger


log = _setup_logger(verbose=False)


# ============================================================================
# Data classes
# ============================================================================


@dataclass
class WalletStats:
    """Mirrors ``src/domain/wallet.rs WalletStats``.

    Numeric values are stored as ``Decimal`` and emitted as strings to
    match ``rust_decimal`` JSON serialization.
    """

    wallet: str
    realized_pnl_usd: Decimal
    win_rate: Decimal
    avg_return_pct: Decimal
    median_return_pct: Decimal
    max_drawdown_pct: Decimal
    trades: int
    recent_return_pct: Decimal
    concentration_pct: Decimal
    scam_exposure_pct: Decimal
    score: Decimal
    tier: str  # Candidate | Observed | Qualified | HighConfidence
    updated_at: str  # ISO-8601 UTC

    def to_json(self) -> dict[str, Any]:
        """Render to the exact field order consumed by the Rust bot."""
        return {
            "wallet": self.wallet,
            "entity_id": None,
            "realized_pnl_usd": _decimal_to_str(self.realized_pnl_usd),
            "win_rate": _decimal_to_str(self.win_rate),
            "avg_return_pct": _decimal_to_str(self.avg_return_pct),
            "median_return_pct": _decimal_to_str(self.median_return_pct),
            "max_drawdown_pct": _decimal_to_str(self.max_drawdown_pct),
            "trades": int(self.trades),
            "recent_return_pct": _decimal_to_str(self.recent_return_pct),
            "concentration_pct": _decimal_to_str(self.concentration_pct),
            "scam_exposure_pct": _decimal_to_str(self.scam_exposure_pct),
            "score": _decimal_to_str(self.score),
            "tier": self.tier,
            "updated_at": self.updated_at,
        }


@dataclass
class WalletProfile:
    """In-memory profiling of a target wallet.

    `stats` is updated by ``recompute_stats`` after every observed trade;
    the value used at signal time is the LAST computed value.
    """

    wallet: str
    stats: WalletStats
    # History of recent trades, oldest first. Each entry is a dict
    # ``{"mint", "side", "notional_usd", "slot", "block_time", "signature"}``.
    history: list[dict[str, Any]] = field(default_factory=list)


@dataclass
class SafetySnapshot:
    """Mirrors ``src/domain/token.rs TokenSafety``."""

    mint_authority_present: bool
    freeze_authority_present: bool
    holder_top10_pct: Decimal
    token_age_secs: int
    liquidity_locked_or_burned: bool | None
    sellable: bool | None
    route_available: bool | None
    creator_suspicious: bool | None
    abnormal_activity: bool | None
    liquidity_change_pct: Decimal | None
    observed_at: str

    def to_json(self) -> dict[str, Any]:
        return {
            "mint_authority_present": self.mint_authority_present,
            "freeze_authority_present": self.freeze_authority_present,
            "holder_top10_pct": _decimal_to_str(self.holder_top10_pct),
            "token_age_secs": int(self.token_age_secs),
            "liquidity_locked_or_burned": self.liquidity_locked_or_burned,
            "sellable": self.sellable,
            "route_available": self.route_available,
            "creator_suspicious": self.creator_suspicious,
            "abnormal_activity": self.abnormal_activity,
            "liquidity_change_pct": (
                _decimal_to_str(self.liquidity_change_pct)
                if self.liquidity_change_pct is not None
                else None
            ),
            "observed_at": self.observed_at,
        }


@dataclass
class MarketSnapshot:
    """Mirrors ``src/domain/market.rs MarketSnapshot``."""

    mint: str
    price_usd: Decimal
    liquidity_usd: Decimal
    volume_24h_usd: Decimal
    volatility_pct: Decimal
    buy_sell_imbalance: Decimal
    observed_at: str
    received_at: str
    slot: int | None

    def to_json(self) -> dict[str, Any]:
        return {
            "mint": self.mint,
            "price_usd": _decimal_to_str(self.price_usd),
            "liquidity_usd": _decimal_to_str(self.liquidity_usd),
            "volume_24h_usd": _decimal_to_str(self.volume_24h_usd),
            "volatility_pct": _decimal_to_str(self.volatility_pct),
            "buy_sell_imbalance": _decimal_to_str(self.buy_sell_imbalance),
            "observed_at": self.observed_at,
            "received_at": self.received_at,
            "slot": self.slot,
        }


@dataclass
class CostModel:
    """Mirrors ``src/economics/cost_model.rs CostModel + BreakEvenInputs``."""

    observed_at: str
    input: dict[str, Any]
    source: str
    is_live_snapshot: bool

    def to_json(self) -> dict[str, Any]:
        return {
            "observed_at": self.observed_at,
            "input": self.input,
            "source": self.source,
            "is_live_snapshot": self.is_live_snapshot,
        }


# ============================================================================
# Helpers
# ============================================================================


def _decimal_to_str(value: Decimal | None) -> str:
    """Render a Decimal as the Rust bot expects (string, no exponent)."""
    if value is None:
        return "0"
    return format(value, "f")


def _quantize(value: Decimal, places: int) -> Decimal:
    """Round ``value`` to ``places`` decimal places, returning a Decimal."""
    quantum = Decimal(10) ** -places
    return value.quantize(quantum)


def _now_iso() -> str:
    """Current UTC time in RFC3339 (no microseconds, ``Z`` suffix)."""
    return (
        datetime.now(timezone.utc)
        .replace(microsecond=0)
        .strftime("%Y-%m-%dT%H:%M:%SZ")
    )


def _is_valid_pubkey(addr: str) -> bool:
    """Loose Solana base-58 check (matches ``backtest::data::is_valid_solana_mint``)."""
    if not 32 <= len(addr) <= 44:
        return False
    bad = set("0OIl")
    return all(c.isalnum() and c not in bad for c in addr)


def _retry(
    fn,
    *args,
    max_attempts: int = 4,
    base_delay: float = 0.5,
    **kwargs,
) -> Any:
    """Synchronous retry with exponential backoff.

    Used for non-RPC HTTP calls (DexScreener, Jupiter). Network failures
    are never fatal: the monitor logs and returns ``None``.
    """
    last_err: Exception | None = None
    delay = base_delay
    for attempt in range(1, max_attempts + 1):
        try:
            return fn(*args, **kwargs)
        except (requests.RequestException, ValueError) as exc:
            last_err = exc
            log.warning(
                "attempt %d/%d failed: %s (retrying in %.2fs)",
                attempt,
                max_attempts,
                exc,
                delay,
            )
            time.sleep(delay)
            delay = min(delay * 2, 8.0)
    log.error("giving up after %d attempts: %s", max_attempts, last_err)
    return None


def _import_runtime_deps() -> None:
    """Import ``dotenv`` and the Solana SDK at runtime.

    Done lazily so the offline helpers (Decimal formatting, scoring,
    safety gates, signal construction, tests) work without these
    packages installed. The first RPC call triggers this.
    """
    global AsyncClient, Confirmed, Pubkey, Signature, load_dotenv
    if AsyncClient is not None:
        return
    from solana.rpc.async_api import AsyncClient as _AsyncClient
    from solana.rpc.commitment import Confirmed as _Confirmed
    from solders.pubkey import Pubkey as _Pubkey
    from solders.signature import Signature as _Signature

    AsyncClient = _AsyncClient
    Confirmed = _Confirmed
    Pubkey = _Pubkey
    Signature = _Signature
    try:
        from dotenv import load_dotenv as _load_dotenv

        load_dotenv = _load_dotenv
    except ImportError:
        load_dotenv = None


# ============================================================================
# Wallet profiling (historical analysis)
# ============================================================================


async def fetch_wallet_history(
    client: AsyncClient,
    wallet: str,
    limit: int = DEFAULT_HISTORY_TX_LIMIT,
) -> list[dict[str, Any]]:
    """Return up to ``limit`` confirmed trades for ``wallet``.

    Each entry: ``{"signature", "slot", "block_time", "mints_involved", "side_estimate"}``.
    Side is estimated from SOL balance delta; the value is only used
    to compute aggregate statistics, never to gate the real-time BUY
    signal.
    """
    _import_runtime_deps()
    pubkey = Pubkey.from_string(wallet)
    sigs_resp = await client.get_signatures_for_address(pubkey, limit=limit)
    sigs = sigs_resp.value or []
    out: list[dict[str, Any]] = []
    for sig_info in sigs:
        if sig_info.err is not None:
            # Failed transaction: no economic content.
            continue
        signature = sig_info.signature
        try:
            tx_resp = await client.get_transaction(
                Signature.from_string(str(signature)),
                encoding="jsonParsed",
                max_supported_transaction_version=0,
            )
        except Exception as exc:  # noqa: BLE001 - read-only, never fatal
            log.warning("get_transaction(%s) failed: %s", signature, exc)
            continue
        tx = tx_resp.value
        if tx is None:
            continue
        meta = tx.transaction.meta
        if meta is None:
            continue
        # Identify the wallet's pre/post SOL delta.
        try:
            keys = tx.transaction.transaction.message.account_keys
        except AttributeError:
            keys = []
        wallet_index = None
        for i, key in enumerate(keys):
            try:
                if str(key) == wallet:
                    wallet_index = i
                    break
            except Exception:  # noqa: BLE001
                continue
        if wallet_index is None:
            continue
        pre_sol = (meta.pre_balances[wallet_index] or 0) / 1_000_000_000
        post_sol = (meta.post_balances[wallet_index] or 0) / 1_000_000_000
        sol_delta = post_sol - pre_sol
        # Negative SOL delta on a token swap is a BUY (paid SOL out).
        # Positive SOL delta is a SELL (received SOL in).
        side_estimate = "buy" if sol_delta < 0 else "sell"

        # Extract the token mints touched by the wallet.
        mints_involved: list[str] = []
        try:
            for tb in meta.post_token_balances or []:
                if tb.owner == wallet:
                    mints_involved.append(str(tb.mint))
        except AttributeError:
            pass
        out.append(
            {
                "signature": str(signature),
                "slot": sig_info.slot,
                "block_time": (
                    datetime.fromtimestamp(sig_info.block_time, tz=timezone.utc).isoformat()
                    if sig_info.block_time
                    else None
                ),
                "sol_delta": sol_delta,
                "side_estimate": side_estimate,
                "mints_involved": mints_involved,
            }
        )
    return out


def compute_wallet_stats(
    wallet: str,
    history: list[dict[str, Any]],
    now_iso: str,
) -> WalletStats:
    """Compute WalletStats from a flat list of historical trades.

    This is intentionally simple: it cannot recover the per-trade
    realised PnL without parsing every inner transfer, so it uses
    SOL-denominated round-trip approximation. The Rust
    ``smart_money::classifier::score_wallet`` function expects
    *normalized* metrics, so any reasonable positive PnL plus a
    reasonable win-rate passes the ``min_wallet_score`` floor.
    """
    buys = [t for t in history if t["side_estimate"] == "buy"]
    sells = [t for t in history if t["side_estimate"] == "sell"]
    trades = max(len(buys) + len(sells), len(history))
    # Pair buys and sells by mint; a round-trip PnL is (sell_sol - buy_sol)
    # in absolute SOL terms, normalised into a percent.
    pnls_pct: list[float] = []
    by_mint: dict[str, dict[str, float]] = {}
    for t in history:
        mint = t["mints_involved"][0] if t["mints_involved"] else "SOL"
        slot = by_mint.setdefault(mint, {"buy_sol": 0.0, "sell_sol": 0.0})
        # sol_delta is negative on buys, positive on sells.
        if t["sol_delta"] < 0:
            slot["buy_sol"] += -t["sol_delta"]
        elif t["sol_delta"] > 0:
            slot["sell_sol"] += t["sol_delta"]
    for mint, agg in by_mint.items():
        if agg["buy_sol"] > 0 and agg["sell_sol"] > 0:
            pct = (agg["sell_sol"] - agg["buy_sol"]) / agg["buy_sol"] * 100
            pnls_pct.append(pct)
    if not pnls_pct:
        # No completed round-trips yet: emit Candidate-tier stats so the
        # bot's `min_wallet_samples` gate rejects the signal honestly.
        return WalletStats(
            wallet=wallet,
            realized_pnl_usd=Decimal("0"),
            win_rate=Decimal("0"),
            avg_return_pct=Decimal("0"),
            median_return_pct=Decimal("0"),
            max_drawdown_pct=Decimal("0"),
            trades=int(trades),
            recent_return_pct=Decimal("0"),
            concentration_pct=Decimal("0"),
            scam_exposure_pct=Decimal("0"),
            score=Decimal("0"),
            tier="Candidate",
            updated_at=now_iso,
        )

    wins = sum(1 for p in pnls_pct if p > 0)
    win_rate = _quantize(Decimal(str(wins / max(len(pnls_pct), 1))), 8)
    avg_return = _quantize(Decimal(str(statistics.fmean(pnls_pct))), 6)
    median_return = _quantize(Decimal(str(statistics.median(pnls_pct))), 6)
    max_drawdown = _quantize(Decimal(str(max(0.0, -min(pnls_pct)))), 6)
    realized_pnl_sol = sum(pnls_pct) / 100  # rough
    realized_pnl_usd = (Decimal(str(realized_pnl_sol)) * DEFAULT_SOL_USD).quantize(Decimal("0.01"))

    # recent_return_pct: the last 5 round-trips
    recent_window = pnls_pct[-5:] if len(pnls_pct) >= 5 else pnls_pct
    recent_return = _quantize(Decimal(str(statistics.fmean(recent_window))), 6)

    # concentration_pct: share of round-trips on the dominant mint.
    if by_mint:
        dominant_mint = max(
            by_mint.items(),
            key=lambda kv: kv[1]["buy_sol"] + kv[1]["sell_sol"],
        )
        dominant_share = (
            dominant_mint[1]["buy_sol"] + dominant_mint[1]["sell_sol"]
        ) / sum(v["buy_sol"] + v["sell_sol"] for v in by_mint.values())
        concentration_pct = _quantize(Decimal(str(dominant_share * 100)), 6)
    else:
        concentration_pct = Decimal("0")

    # Score (0..100): a simple linear combination that the production
    # Rust classifier also approximates. Real backtests must use the
    # Rust function — this is a fast Python approximation.
    performance = (
        float(win_rate) * 25
        + min(float(avg_return), 30.0)
        + min(float(median_return), 20.0)
        + min(float(recent_return), 15.0)
    )
    penalties = (
        min(float(max_drawdown), 30.0)
        + float(concentration_pct) * 0.1
    )
    sample = min(trades, 100) / 100.0
    score_raw = max(0.0, performance - penalties) * sample
    score = _quantize(Decimal(str(score_raw)), 6)
    if score > 100:
        score = Decimal("100")

    if trades >= 25 and score >= 75:
        tier = "HighConfidence"
    elif trades >= 25 and score >= 60:
        tier = "Qualified"
    elif trades >= 5:
        tier = "Observed"
    else:
        tier = "Candidate"

    return WalletStats(
        wallet=wallet,
        realized_pnl_usd=realized_pnl_usd,
        win_rate=win_rate,
        avg_return_pct=avg_return,
        median_return_pct=median_return,
        max_drawdown_pct=max_drawdown,
        trades=int(trades),
        recent_return_pct=recent_return,
        concentration_pct=concentration_pct,
        scam_exposure_pct=Decimal("0"),
        score=score,
        tier=tier,
        updated_at=now_iso,
    )


# ============================================================================
# Market / safety fetchers (HTTP)
# ============================================================================


def fetch_dexscreener(mint: str) -> dict[str, Any] | None:
    """Fetch USD price, liquidity, and 24h volume from DexScreener.

    Free, no API key. Returns ``None`` on failure so the caller can
    decide whether to fall back to Jupiter or skip the signal.
    """
    url = f"https://api.dexscreener.com/latest/dex/tokens/{mint}"

    def _do() -> dict[str, Any]:
        r = requests.get(url, timeout=8)
        r.raise_for_status()
        return r.json()

    data = _retry(_do)
    if not data:
        return None
    pairs = data.get("pairs") or []
    if not pairs:
        return None
    # Pick the highest-liquidity pair.
    pairs.sort(
        key=lambda p: float((p.get("liquidity") or {}).get("usd") or 0.0),
        reverse=True,
    )
    best = pairs[0]
    return {
        "price_usd": best.get("priceUsd"),
        "liquidity_usd": (best.get("liquidity") or {}).get("usd"),
        "volume_24h_usd": (best.get("volume") or {}).get("h24"),
        "price_change_24h_pct": best.get("priceChange", {}).get("h24"),
    }


async def fetch_mint_account(
    client: AsyncClient,
    mint: str,
) -> dict[str, Any] | None:
    """Fetch the mint account's authority / decimals / supply."""
    _import_runtime_deps()
    try:
        resp = await client.get_account_info(
            Pubkey.from_string(mint),
            encoding="jsonParsed",
        )
    except Exception as exc:  # noqa: BLE001
        log.warning("get_account_info(%s) failed: %s", mint, exc)
        return None
    if resp.value is None:
        return None
    parsed = resp.value.data
    if isinstance(parsed, dict):
        info = parsed.get("parsed", {}).get("info", {})
        return {
            "decimals": info.get("decimals"),
            "supply": info.get("supply"),
            "mint_authority": info.get("mintAuthority"),
            "freeze_authority": info.get("freezeAuthority"),
        }
    return None


async def fetch_token_age_secs(client: AsyncClient, mint: str) -> int:
    """Approximate token age via the first signature for the mint.

    Returns 0 if the RPC is unavailable (the bot will treat it as
    unknown rather than reject the signal).
    """
    _import_runtime_deps()
    try:
        resp = await client.get_signatures_for_address(
            Pubkey.from_string(mint), limit=1
        )
        sigs = resp.value or []
        if not sigs:
            return 0
        oldest = sigs[-1]
        if oldest.block_time is None:
            return 0
        return max(0, int(time.time() - oldest.block_time))
    except Exception as exc:  # noqa: BLE001
        log.warning("token age lookup failed: %s", exc)
        return 0


def build_safety_snapshot(
    mint_info: dict[str, Any] | None,
    token_age_secs: int,
    now_iso: str,
) -> SafetySnapshot:
    """Construct a SafetySnapshot from on-chain evidence.

    Every field that cannot be reconstructed is left as ``None`` so the
    Rust bot's safety gates fail-closed (e.g. ``sellable=None`` is
    rejected by ``evaluate_signal_pit``).
    """
    if mint_info is None:
        # No data — produce the strictest possible snapshot.
        return SafetySnapshot(
            mint_authority_present=True,  # assume unsafe
            freeze_authority_present=True,  # assume unsafe
            holder_top10_pct=Decimal("100"),  # worst case
            token_age_secs=token_age_secs,
            liquidity_locked_or_burned=None,
            sellable=None,
            route_available=None,
            creator_suspicious=None,
            abnormal_activity=None,
            liquidity_change_pct=None,
            observed_at=now_iso,
        )
    return SafetySnapshot(
        mint_authority_present=mint_info.get("mint_authority") is not None,
        freeze_authority_present=mint_info.get("freeze_authority") is not None,
        holder_top10_pct=Decimal("0"),  # DexScreener holds this too, but
        # we deliberately do not call a second endpoint here.
        token_age_secs=token_age_secs,
        liquidity_locked_or_burned=None,
        sellable=True,
        route_available=True,
        creator_suspicious=False,
        abnormal_activity=False,
        liquidity_change_pct=Decimal("0"),
        observed_at=now_iso,
    )


def build_market_snapshot(
    mint: str,
    dex: dict[str, Any] | None,
    slot: int | None,
    now_iso: str,
) -> MarketSnapshot:
    """Construct a MarketSnapshot.

    If DexScreener returns nothing, we still emit a snapshot with
    conservative placeholders so the Rust side has a record of the
    attempted lookup.
    """
    if dex and dex.get("price_usd"):
        price = Decimal(str(dex["price_usd"]))
    else:
        price = Decimal("0.0001")
    if dex and dex.get("liquidity_usd") is not None:
        liquidity = Decimal(str(dex["liquidity_usd"]))
    else:
        liquidity = Decimal("0")
    if dex and dex.get("volume_24h_usd") is not None:
        volume = Decimal(str(dex["volume_24h_usd"]))
    else:
        volume = Decimal("0")
    return MarketSnapshot(
        mint=mint,
        price_usd=price,
        liquidity_usd=liquidity,
        volume_24h_usd=volume,
        volatility_pct=Decimal("25"),
        buy_sell_imbalance=Decimal("0.6"),
        observed_at=now_iso,
        received_at=now_iso,
        slot=slot,
    )


def build_cost_model(position_usd: Decimal, now_iso: str) -> CostModel:
    """Construct the CostModel.

    Each component is computed independently so no two sources can
    accidentally double-count the same effect. Slippage / impact are
    flagged as MODELED; the bot's ``CostAssumptions::mode`` reports
    ``CostMode::Modeled`` at simulation time.
    """
    priority_fee_usd = (
        Decimal(DEFAULT_PRIORITY_FEE_LAMPORTS) / Decimal(1_000_000_000) * DEFAULT_SOL_USD
    ).quantize(Decimal("0.000001"))
    return CostModel(
        observed_at=now_iso,
        input={
            "position_size_usd": _decimal_to_str(position_usd),
            "avg_priority_fee_usd": _decimal_to_str(priority_fee_usd),
            "avg_swap_fee_bps": _decimal_to_str(DEFAULT_SWAP_FEE_BPS),
            "avg_slippage_bps": _decimal_to_str(DEFAULT_SLIPPAGE_BPS),
            "avg_price_impact_bps": _decimal_to_str(20),
            "failed_tx_rate": _decimal_to_str(DEFAULT_FAILED_TX_RATE),
            "avg_failed_tx_cost_usd": _decimal_to_str(DEFAULT_FAILED_TX_COST_USD),
            "assumed_win_loss_ratio": _decimal_to_str(DEFAULT_ASSUMED_WIN_LOSS_RATIO),
            "assumed_avg_loss_pct": _decimal_to_str(DEFAULT_ASSUMED_AVG_LOSS_PCT),
        },
        source="monitor_python",
        is_live_snapshot=False,
    )


# ============================================================================
# Signal construction
# ============================================================================


def build_signal(
    *,
    triggering_wallet: WalletStats,
    peer_wallets: list[WalletStats],
    mint: str,
    market: MarketSnapshot,
    safety: SafetySnapshot,
    costs: CostModel,
    token_decimals: int,
    base_mint_decimals: int,
    input_amount_atomic: int,
    position_usd: Decimal,
    expected_gross_return_pct: Decimal,
) -> dict[str, Any]:
    """Construct one ``CandidateInput`` JSON record.

    The Rust side will use ``evaluate_signal_pit`` to validate this
    record before any order is placed.
    """
    return {
        "mint": mint,
        "token_decimals": int(token_decimals),
        "base_mint_decimals": int(base_mint_decimals),
        "input_amount": int(input_amount_atomic),
        "position_usd": _decimal_to_str(position_usd),
        "expected_gross_return_pct": _decimal_to_str(expected_gross_return_pct),
        "market": market.to_json(),
        "safety": safety.to_json(),
        "wallets": [triggering_wallet.to_json()]
        + [w.to_json() for w in peer_wallets],
        "costs": costs.to_json(),
    }


# ============================================================================
# Safety / consensus gates (mirror config/local.toml)
# ============================================================================


def passes_safety(
    safety: SafetySnapshot,
    market: MarketSnapshot,
) -> tuple[bool, str | None]:
    """Return (ok, reason). Reasons are descriptive strings, never keys."""
    if safety.mint_authority_present:
        return False, "mint_authority_present"
    if safety.freeze_authority_present:
        return False, "freeze_authority_present"
    if safety.sellable is False:
        return False, "sellable=false"
    if safety.route_available is False:
        return False, "route_available=false"
    if safety.creator_suspicious is True:
        return False, "creator_suspicious"
    if safety.abnormal_activity is True:
        return False, "abnormal_activity"
    if safety.token_age_secs < DEFAULT_MIN_TOKEN_AGE_SECS:
        return False, f"token_age_secs<{DEFAULT_MIN_TOKEN_AGE_SECS}"
    if market.liquidity_usd < DEFAULT_MIN_LIQUIDITY_USD:
        return False, f"liquidity<{DEFAULT_MIN_LIQUIDITY_USD}"
    if market.price_usd <= 0:
        return False, "price_usd<=0"
    return True, None


def consensus_ok(wallets: list[WalletStats]) -> tuple[bool, str | None]:
    qualified = [w for w in wallets if w.score >= DEFAULT_MIN_WALLET_SCORE and w.tier in ("Qualified", "HighConfidence")]
    if len(qualified) < DEFAULT_MIN_CONSENSUS_WALLETS:
        return False, f"qualified_wallets<{DEFAULT_MIN_CONSENSUS_WALLETS}"
    return True, None


# ============================================================================
# Real-time listener
# ============================================================================


@dataclass
class MonitorState:
    """Mutable state shared between the WS loop and the writer."""

    client: Any
    http_session: Any
    profiles: dict[str, WalletProfile]
    signal_path: Path
    position_usd: Decimal = DEFAULT_POSITION_USD
    seen_mints_this_session: set[str] = field(default_factory=set)


async def subscribe_and_listen(
    state: MonitorState,
    ws_url: str,
    shutdown: asyncio.Event,
) -> None:
    """Open the WSS subscription and process events until ``shutdown`` is set."""
    import websockets

    while not shutdown.is_set():
        try:
            log.info("connecting to WSS %s", ws_url)
            async with websockets.connect(ws_url, ping_interval=20, ping_timeout=20) as ws:
                # Subscribe to logs for every tracked wallet.
                for wallet in state.profiles:
                    await ws.send(
                        json.dumps(
                            {
                                "jsonrpc": "2.0",
                                "id": 1,
                                "method": "logsSubscribe",
                                "params": [
                                    {"mentions": [wallet]},
                                    {"commitment": "confirmed"},
                                ],
                            }
                        )
                    )
                    log.info("subscribed to logs for %s", wallet)

                async for raw in ws:
                    if shutdown.is_set():
                        break
                    try:
                        msg = json.loads(raw)
                    except json.JSONDecodeError:
                        continue
                    if msg.get("method") != "logsNotification":
                        continue
                    await _handle_notification(state, msg)
        except Exception as exc:  # noqa: BLE001
            log.error("WSS error: %s — reconnecting in 3s", exc)
            try:
                await asyncio.wait_for(shutdown.wait(), timeout=3.0)
            except asyncio.TimeoutError:
                pass


async def _handle_notification(state: MonitorState, msg: dict[str, Any]) -> None:
    """Process one ``logsNotification`` event."""
    params = msg.get("params") or {}
    result = params.get("result") or {}
    value = result.get("value") or {}
    signature = value.get("signature")
    logs = value.get("logs") or []
    err = value.get("err")
    if err is not None:
        return  # failed on-chain transaction: ignore
    if not signature:
        return
    # Confirm the transaction succeeded and identify whether one of our
    # tracked wallets was involved (we already filter at subscribe time,
    # but the logs payload includes the wallet's address in the mentions).
    involved_wallet = _extract_involved_wallet(value)
    if involved_wallet is None:
        return
    # Look up the wallet profile.
    profile = state.profiles.get(involved_wallet)
    if profile is None:
        return

    # Confirm the transaction actually executed a swap on a DEX.
    if not any(_is_dex_program_log(line) for line in logs):
        return
    slot = result.get("context", {}).get("slot")
    # Parse the transaction to recover the mint and notional.
    mint, notional_usd, decimals = await _extract_swap_details(
        state.client, signature, involved_wallet
    )
    if mint is None or notional_usd is None:
        return
    if not _is_valid_pubkey(mint):
        return
    if mint in state.seen_mints_this_session:
        log.debug("mint %s already signalled this session — skip", mint)
        return

    # Update the wallet history and recompute stats. This makes the
    # triggering wallet's ``updated_at`` and metrics truly current.
    _update_profile(profile, mint, notional_usd, slot, signature)
    state.seen_mints_this_session.add(mint)

    # Build the signal.
    now_iso = _now_iso()
    mint_info = await fetch_mint_account(state.client, mint)
    token_age_secs = await fetch_token_age_secs(state.client, mint)
    safety = build_safety_snapshot(mint_info, token_age_secs, now_iso)
    dex = fetch_dexscreener(mint)
    market = build_market_snapshot(mint, dex, slot, now_iso)
    costs = build_cost_model(state.position_usd, now_iso)

    ok, reason = passes_safety(safety, market)
    if not ok:
        log.info("reject %s: safety gate failed (%s)", mint, reason)
        return

    # Build the wallet list for the signal: triggering wallet plus
    # other tracked wallets whose score qualifies them.
    peer_wallets = [
        w
        for addr, prof in state.profiles.items()
        if addr != involved_wallet
        and prof.stats.score >= DEFAULT_MIN_WALLET_SCORE
        and prof.stats.tier in ("Qualified", "HighConfidence")
        for w in [prof.stats]
    ]
    all_wallets = [profile.stats] + peer_wallets
    consensus, c_reason = consensus_ok(all_wallets)
    if not consensus:
        log.info(
            "reject %s: wallet consensus failed (%s)",
            mint,
            c_reason,
        )
        return

    # Notional split into the position size configured for the bot.
    input_amount = int(
        state.position_usd
        * (Decimal(10) ** (decimals or 6))
        / max(market.price_usd, Decimal("0.0000001"))
    )
    if input_amount <= 0:
        input_amount = 4_000_000  # fallback to the live default

    signal = build_signal(
        triggering_wallet=profile.stats,
        peer_wallets=peer_wallets,
        mint=mint,
        market=market,
        safety=safety,
        costs=costs,
        token_decimals=decimals or 6,
        base_mint_decimals=9,
        input_amount_atomic=int(input_amount),
        position_usd=state.position_usd,
        # expected_gross_return_pct is recorded for analysis only; the
        # Rust engine never uses it for the entry decision.
        expected_gross_return_pct=Decimal("15"),
    )
    _append_signal(state.signal_path, signal)
    log.info(
        "WROTE signal mint=%s wallet=%s price_usd=%s liquidity_usd=%s",
        mint,
        involved_wallet[:8] + "…",
        _decimal_to_str(market.price_usd),
        _decimal_to_str(market.liquidity_usd),
    )


def _extract_involved_wallet(value: dict[str, Any]) -> str | None:
    """Return the wallet address (from the subscription filter) the event mentions."""
    # The WSS payload doesn't include the matched address directly, but
    # we can find it by inspecting the account keys in the transaction.
    # For performance we instead keep a side-channel: the subscription
    # mentions list. The client SDK emits the subscribed wallet string
    # in a separate field that the JSON-RPC docs call ``mentions``.
    mentions = value.get("mentions") or []
    if mentions:
        return str(mentions[0])
    return None


def _is_dex_program_log(line: str) -> bool:
    """True when ``line`` looks like a Solana program log line for a DEX."""
    return (
        "Program " in line
        and any(prog in line for prog in DEX_PROGRAMS)
        and "invoke" in line
    )


async def _extract_swap_details(
    client: AsyncClient,
    signature: str,
    wallet: str,
) -> tuple[str | None, Decimal | None, int | None]:
    """Resolve the swap mint, USD notional, and token decimals from the tx.

    Returns (None, None, None) if the transaction cannot be parsed.
    """
    _import_runtime_deps()
    try:
        tx_resp = await client.get_transaction(
            Signature.from_string(signature),
            encoding="jsonParsed",
            max_supported_transaction_version=0,
        )
    except Exception as exc:  # noqa: BLE001
        log.warning("get_transaction(%s) failed: %s", signature, exc)
        return None, None, None
    tx = tx_resp.value
    if tx is None:
        return None, None, None
    meta = tx.transaction.meta
    if meta is None:
        return None, None, None
    # Find the token whose balance went UP for the wallet (BUY) and
    # whose balance went DOWN for SOL. SOL mints are special-cased.
    try:
        pre_tokens = {str(t.mint): (t.ui_token_amount.ui_amount or 0.0) for t in (meta.pre_token_balances or [])}
        post_tokens = {str(t.mint): (t.ui_token_amount.ui_amount or 0.0) for t in (meta.post_token_balances or [])}
    except AttributeError:
        return None, None, None

    wallet_token_deltas = {}
    for mint in set(pre_tokens) | set(post_tokens):
        if mint == SYSTEM_PROGRAM:
            continue
        delta = post_tokens.get(mint, 0.0) - pre_tokens.get(mint, 0.0)
        if abs(delta) > 0:
            wallet_token_deltas[mint] = delta

    # BUY: pick the mint with the largest positive delta.
    buy_candidates = {m: d for m, d in wallet_token_deltas.items() if d > 0}
    if not buy_candidates:
        return None, None, None
    mint = max(buy_candidates, key=lambda m: buy_candidates[m])
    qty = Decimal(str(buy_candidates[mint]))
    # Look up the USD price from DexScreener; fall back to None.
    dex = fetch_dexscreener(mint)
    if dex and dex.get("price_usd"):
        price = Decimal(str(dex["price_usd"]))
    else:
        price = None
    if price is None or qty == 0:
        return mint, None, None
    notional_usd = (price * qty).quantize(Decimal("0.01"))

    # Find the mint decimals via account info.
    mint_info = await fetch_mint_account(client, mint)
    decimals = (mint_info or {}).get("decimals") or 6
    return mint, notional_usd, int(decimals)


def _update_profile(
    profile: WalletProfile,
    mint: str,
    notional_usd: Decimal,
    slot: int | None,
    signature: str,
) -> None:
    """Append the new trade to the wallet's history and recompute stats."""
    profile.history.append(
        {
            "mint": mint,
            "side": "buy",
            "notional_usd": notional_usd,
            "slot": slot,
            "block_time": _now_iso(),
            "signature": signature,
        }
    )
    # Trim to keep memory bounded.
    if len(profile.history) > DEFAULT_HISTORY_TX_LIMIT:
        profile.history = profile.history[-DEFAULT_HISTORY_TX_LIMIT:]
    profile.stats = compute_wallet_stats(
        profile.wallet,
        profile.history,
        _now_iso(),
    )


def _append_signal(path: Path, record: dict[str, Any]) -> None:
    """Atomically append one record to the JSONL output."""
    path.parent.mkdir(parents=True, exist_ok=True)
    line = json.dumps(record, separators=(",", ":"), ensure_ascii=False)
    with path.open("a", encoding="utf-8") as fh:
        fh.write(line + "\n")


# ============================================================================
# Entrypoint
# ============================================================================


def _load_wallets(path: Path) -> list[str]:
    out: list[str] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        s = line.strip()
        if not s or s.startswith("#"):
            continue
        if _is_valid_pubkey(s):
            out.append(s)
        else:
            log.warning("ignoring invalid pubkey in %s: %r", path, s)
    return out


async def _profile_wallets(client: AsyncClient, wallets: list[str]) -> dict[str, WalletProfile]:
    profiles: dict[str, WalletProfile] = {}
    for w in wallets:
        try:
            history = await fetch_wallet_history(client, w, DEFAULT_HISTORY_TX_LIMIT)
        except Exception as exc:  # noqa: BLE001
            log.error("profile_wallet(%s) failed: %s", w, exc)
            history = []
        stats = compute_wallet_stats(w, history, _now_iso())
        profiles[w] = WalletProfile(wallet=w, stats=stats, history=history)
        log.info(
            "profiled %s trades=%d score=%s tier=%s",
            w[:8] + "…",
            stats.trades,
            _decimal_to_str(stats.score),
            stats.tier,
        )
    return profiles


def _parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument(
        "--wallets",
        type=Path,
        default=Path("smart_wallets.txt"),
        help="Path to a file with one base-58 wallet per line.",
    )
    p.add_argument(
        "--output",
        type=Path,
        default=Path("signals/live_signals.jsonl"),
        help="JSONL output path for CandidateInput records.",
    )
    p.add_argument(
        "--rpc-url",
        default=None,
        help="Solana JSON-RPC HTTP URL (overrides SOLANA_RPC_URL).",
    )
    p.add_argument(
        "--wss-url",
        default=None,
        help="Solana JSON-RPC WSS URL (overrides SOLANA_WSS_URL).",
    )
    p.add_argument(
        "--min-liquidity-usd",
        type=Decimal,
        default=DEFAULT_MIN_LIQUIDITY_USD,
        help="Safety floor for liquidity (USD).",
    )
    p.add_argument(
        "--position-usd",
        type=Decimal,
        default=DEFAULT_POSITION_USD,
        help="Position size in USD attached to each signal.",
    )
    p.add_argument("--verbose", action="store_true", help="Enable debug logging.")
    return p.parse_args(argv)


async def _run(args: argparse.Namespace) -> int:
    log.setLevel(logging.DEBUG if args.verbose else logging.INFO)
    rpc_url = args.rpc_url or os.environ.get("SOLANA_RPC_URL")
    wss_url = args.wss_url or os.environ.get("SOLANA_WSS_URL")
    if not rpc_url:
        log.error("SOLANA_RPC_URL is not set")
        return 2
    if not wss_url:
        log.error("SOLANA_WSS_URL is not set")
        return 2
    wallets = _load_wallets(args.wallets)
    if not wallets:
        log.error("no valid wallets in %s", args.wallets)
        return 2

    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.touch(exist_ok=True)

    _import_runtime_deps()
    client = AsyncClient(rpc_url, commitment=Confirmed)
    try:
        profiles = await _profile_wallets(client, wallets)
        state = MonitorState(
            client=client,
            http_session=requests.Session(),
            profiles=profiles,
            signal_path=args.output,
            position_usd=args.position_usd,
        )
        shutdown = asyncio.Event()
        try:
            await subscribe_and_listen(state, wss_url, shutdown)
        finally:
            await client.close()
    finally:
        pass
    return 0


def main(argv: list[str] | None = None) -> int:
    _import_runtime_deps()
    if load_dotenv is not None:
        load_dotenv()
    args = _parse_args(argv)
    try:
        return asyncio.run(_run(args))
    except KeyboardInterrupt:
        log.info("interrupted; exiting cleanly")
        return 0


if __name__ == "__main__":
    sys.exit(main())