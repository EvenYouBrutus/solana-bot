"""Offline smoke tests for monitor.py.

These tests exercise the pure-Python helpers (Decimal serialization,
wallet scoring, pubkey validation, safety gates, signal construction).
They never touch the network so they run without any API keys.

Run::

    python monitor_tests.py
"""

from __future__ import annotations

import sys
from decimal import Decimal
from pathlib import Path

# Import the module under test directly (not as a package).
sys.path.insert(0, str(Path(__file__).resolve().parent))
import monitor  # noqa: E402


def test_decimal_to_str() -> None:
    assert monitor._decimal_to_str(Decimal("4")) == "4"
    assert monitor._decimal_to_str(Decimal("0.0001")) == "0.0001"
    assert monitor._decimal_to_str(None) == "0"
    # No exponent form.
    assert "E" not in monitor._decimal_to_str(Decimal("0.00000001"))


def test_is_valid_pubkey() -> None:
    assert monitor._is_valid_pubkey("5kqEvH3gnx5HUYA8UmK3Za5gF3kRpY3oUg3TCY4tJhPb")
    assert not monitor._is_valid_pubkey("")  # too short
    assert not monitor._is_valid_pubkey("0invalid0000000000000000000000000000000")  # contains 0
    assert not monitor._is_valid_pubkey("Iinvalid0000000000000000000000000000000")  # contains I
    assert not monitor._is_valid_pubkey("linvalid0000000000000000000000000000000")  # contains l
    assert not monitor._is_valid_pubkey("Oinvalid0000000000000000000000000000000")  # contains O
    assert not monitor._is_valid_pubkey("short")  # too short


def test_now_iso_is_utc_z_suffix() -> None:
    s = monitor._now_iso()
    assert s.endswith("Z")
    assert "T" in s


def test_compute_wallet_stats_no_trades() -> None:
    stats = monitor.compute_wallet_stats("w", [], "2024-01-01T00:00:00Z")
    assert stats.tier == "Candidate"
    assert stats.trades == 0
    assert stats.score == Decimal("0")


def test_compute_wallet_stats_with_profitable_history() -> None:
    history = []
    for i in range(50):
        history.append({
            "signature": f"s{i}",
            "slot": 0,
            "block_time": None,
            "sol_delta": -0.1,
            "side_estimate": "buy",
            "mints_involved": ["Mint1111111111111111111111111111111111111"],
        })
        history.append({
            "signature": f"s{i}_sell",
            "slot": 0,
            "block_time": None,
            "sol_delta": 0.15,
            "side_estimate": "sell",
            "mints_involved": ["Mint1111111111111111111111111111111111111"],
        })
    stats = monitor.compute_wallet_stats("w", history, "2024-01-01T00:00:00Z")
    assert stats.trades == 100
    # All round-trips profitable: +50% each.
    assert stats.realized_pnl_usd > Decimal("0")
    assert stats.win_rate > Decimal("0.9")
    # 100 trades × performance ~90 × sample=1.0 → score ~90 → HighConfidence
    assert stats.tier == "HighConfidence"


def test_passes_safety_rejects_mint_authority() -> None:
    safety = monitor.SafetySnapshot(
        mint_authority_present=True,
        freeze_authority_present=False,
        holder_top10_pct=Decimal("20"),
        token_age_secs=86400 * 7,
        liquidity_locked_or_burned=None,
        sellable=None,
        route_available=None,
        creator_suspicious=None,
        abnormal_activity=None,
        liquidity_change_pct=None,
        observed_at="2024-01-01T00:00:00Z",
    )
    market = monitor.MarketSnapshot(
        mint="Mint1111111111111111111111111111111111111",
        price_usd=Decimal("0.001"),
        liquidity_usd=Decimal("100000"),
        volume_24h_usd=Decimal("0"),
        volatility_pct=Decimal("20"),
        buy_sell_imbalance=Decimal("0.5"),
        observed_at="2024-01-01T00:00:00Z",
        received_at="2024-01-01T00:00:00Z",
        slot=0,
    )
    ok, reason = monitor.passes_safety(safety, market)
    assert not ok
    assert reason == "mint_authority_present"


def test_passes_safety_rejects_low_liquidity() -> None:
    safety = monitor.SafetySnapshot(
        mint_authority_present=False,
        freeze_authority_present=False,
        holder_top10_pct=Decimal("20"),
        token_age_secs=86400 * 7,
        liquidity_locked_or_burned=None,
        sellable=True,
        route_available=True,
        creator_suspicious=False,
        abnormal_activity=False,
        liquidity_change_pct=None,
        observed_at="2024-01-01T00:00:00Z",
    )
    market = monitor.MarketSnapshot(
        mint="Mint1111111111111111111111111111111111111",
        price_usd=Decimal("0.001"),
        liquidity_usd=Decimal("1000"),
        volume_24h_usd=Decimal("0"),
        volatility_pct=Decimal("20"),
        buy_sell_imbalance=Decimal("0.5"),
        observed_at="2024-01-01T00:00:00Z",
        received_at="2024-01-01T00:00:00Z",
        slot=0,
    )
    ok, reason = monitor.passes_safety(safety, market)
    assert not ok
    assert "liquidity<" in (reason or "")


def test_passes_safety_accepts_clean_token() -> None:
    safety = monitor.SafetySnapshot(
        mint_authority_present=False,
        freeze_authority_present=False,
        holder_top10_pct=Decimal("20"),
        token_age_secs=86400 * 30,
        liquidity_locked_or_burned=None,
        sellable=True,
        route_available=True,
        creator_suspicious=False,
        abnormal_activity=False,
        liquidity_change_pct=None,
        observed_at="2024-01-01T00:00:00Z",
    )
    market = monitor.MarketSnapshot(
        mint="Mint1111111111111111111111111111111111111",
        price_usd=Decimal("0.001"),
        liquidity_usd=Decimal("200000"),
        volume_24h_usd=Decimal("50000"),
        volatility_pct=Decimal("20"),
        buy_sell_imbalance=Decimal("0.5"),
        observed_at="2024-01-01T00:00:00Z",
        received_at="2024-01-01T00:00:00Z",
        slot=0,
    )
    ok, _ = monitor.passes_safety(safety, market)
    assert ok


def test_consensus_requires_multiple_qualified_wallets() -> None:
    qualified = monitor.WalletStats(
        wallet="Wallet1111111111111111111111111111111111111",
        realized_pnl_usd=Decimal("1000"),
        win_rate=Decimal("0.7"),
        avg_return_pct=Decimal("15"),
        median_return_pct=Decimal("12"),
        max_drawdown_pct=Decimal("10"),
        trades=50,
        recent_return_pct=Decimal("10"),
        concentration_pct=Decimal("5"),
        scam_exposure_pct=Decimal("0"),
        score=Decimal("80"),
        tier="Qualified",
        updated_at="2024-01-01T00:00:00Z",
    )
    candidate = monitor.WalletStats(
        wallet="Wallet2222222222222222222222222222222222222",
        realized_pnl_usd=Decimal("0"),
        win_rate=Decimal("0"),
        avg_return_pct=Decimal("0"),
        median_return_pct=Decimal("0"),
        max_drawdown_pct=Decimal("0"),
        trades=2,
        recent_return_pct=Decimal("0"),
        concentration_pct=Decimal("0"),
        scam_exposure_pct=Decimal("0"),
        score=Decimal("30"),
        tier="Candidate",
        updated_at="2024-01-01T00:00:00Z",
    )
    ok, reason = monitor.consensus_ok([qualified, candidate])
    assert not ok
    assert "qualified_wallets<" in (reason or "")
    ok, _ = monitor.consensus_ok([qualified, qualified])
    assert ok


def test_build_signal_matches_candidate_input_schema() -> None:
    """The produced record must contain every field the Rust bot expects."""
    triggering = monitor.WalletStats(
        wallet="5kqEvH3gnx5HUYA8UmK3Za5gF3kRpY3oUg3TCY4tJhPb",
        realized_pnl_usd=Decimal("1000"),
        win_rate=Decimal("0.7"),
        avg_return_pct=Decimal("15"),
        median_return_pct=Decimal("12"),
        max_drawdown_pct=Decimal("20"),
        trades=50,
        recent_return_pct=Decimal("10"),
        concentration_pct=Decimal("5"),
        scam_exposure_pct=Decimal("0"),
        score=Decimal("80"),
        tier="Qualified",
        updated_at="2024-01-01T00:00:00Z",
    )
    market = monitor.MarketSnapshot(
        mint="Mint1111111111111111111111111111111111111",
        price_usd=Decimal("0.001"),
        liquidity_usd=Decimal("100000"),
        volume_24h_usd=Decimal("50000"),
        volatility_pct=Decimal("20"),
        buy_sell_imbalance=Decimal("0.5"),
        observed_at="2024-01-01T00:00:00Z",
        received_at="2024-01-01T00:00:00Z",
        slot=300000000,
    )
    safety = monitor.SafetySnapshot(
        mint_authority_present=False,
        freeze_authority_present=False,
        holder_top10_pct=Decimal("35"),
        token_age_secs=86400 * 30,
        liquidity_locked_or_burned=None,
        sellable=True,
        route_available=True,
        creator_suspicious=False,
        abnormal_activity=False,
        liquidity_change_pct=None,
        observed_at="2024-01-01T00:00:00Z",
    )
    costs = monitor.build_cost_model(Decimal("4"), "2024-01-01T00:00:00Z")
    sig = monitor.build_signal(
        triggering_wallet=triggering,
        peer_wallets=[triggering],
        mint="Mint1111111111111111111111111111111111111",
        market=market,
        safety=safety,
        costs=costs,
        token_decimals=6,
        base_mint_decimals=9,
        input_amount_atomic=4_000_000,
        position_usd=Decimal("4"),
        expected_gross_return_pct=Decimal("15"),
    )
    # All top-level fields present.
    required = {
        "mint",
        "token_decimals",
        "base_mint_decimals",
        "input_amount",
        "position_usd",
        "expected_gross_return_pct",
        "market",
        "safety",
        "wallets",
        "costs",
    }
    assert required <= sig.keys()
    # Numerics are strings.
    assert isinstance(sig["position_usd"], str)
    assert isinstance(sig["expected_gross_return_pct"], str)
    assert isinstance(sig["market"]["price_usd"], str)
    assert isinstance(sig["costs"]["input"]["position_size_usd"], str)
    # Booleans are native JSON booleans.
    assert isinstance(sig["safety"]["mint_authority_present"], bool)
    # Wallets list matches the candidate schema.
    for w in sig["wallets"]:
        assert "wallet" in w
        assert "tier" in w
        assert "score" in w


def test_load_wallets_skips_invalid() -> None:
    p = Path("__test_wallets.txt")
    p.write_text(
        "# comment line\n"
        "\n"
        "5kqEvH3gnx5HUYA8UmK3Za5gF3kRpY3oUg3TCY4tJhPb\n"
        "0INVALID00000000000000000000000000000000\n"
        "short\n"
        "7xKXtg2CW87d97TXJSDpbD5jBkheTqA83TZRuJosgAsU\n",
        encoding="utf-8",
    )
    try:
        wallets = monitor._load_wallets(p)
        assert len(wallets) == 2
        assert "5kqEvH3gnx5HUYA8UmK3Za5gF3kRpY3oUg3TCY4tJhPb" in wallets
        assert "7xKXtg2CW87d97TXJSDpbD5jBkheTqA83TZRuJosgAsU" in wallets
    finally:
        p.unlink(missing_ok=True)


def main() -> int:
    tests = [v for k, v in globals().items() if k.startswith("test_")]
    failures = 0
    for t in tests:
        try:
            t()
        except AssertionError as exc:
            failures += 1
            print(f"FAIL {t.__name__}: {exc}")
        except Exception as exc:  # noqa: BLE001
            failures += 1
            print(f"ERROR {t.__name__}: {exc}")
        else:
            print(f"OK   {t.__name__}")
    print(f"\n{len(tests) - failures}/{len(tests)} tests passed")
    return 0 if failures == 0 else 1


if __name__ == "__main__":
    sys.exit(main())