#!/usr/bin/env python3
"""
test_parser_local.py – Verify the Python swap parser matches the Rust parser
by testing against the real mainnet fixture file.
"""
import json
import sys
sys.path.insert(0, "tools")

from build_smoke_wallets import (
    parse_swap_from_transaction,
    WalletAccumulator,
    WSOL_MINT,
    COMPLETED_TRADES_THRESHOLD,
)

def test_real_fixture():
    """Test against data/fixtures/real_jupiter_v6_swap.json"""
    with open("data/fixtures/real_jupiter_v6_swap.json") as f:
        tx = json.load(f)

    wallet = "8o29iwRGc3XDe9139HBWsh5ykqHL3K8rpQ2YQGSF9uLo"
    swap = parse_swap_from_transaction(tx, wallet)

    assert swap is not None, "Real mainnet swap must parse"
    assert swap.wallet == wallet
    assert swap.dex == "jupiter_v6", f"Expected jupiter_v6, got {swap.dex}"
    assert swap.direction == "Buy", f"Expected Buy, got {swap.direction}"
    assert swap.input_mint == WSOL_MINT
    assert swap.output_mint == "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB"
    # Output amount should match fixture
    assert swap.output_amount == 41_826_304, f"Expected 41826304, got {swap.output_amount}"
    assert swap.input_amount > 380_000_000
    assert swap.input_amount < 410_000_000
    print(f"PASS: Real fixture - {swap.dex} {swap.direction} {swap.output_amount}")

def test_buy_swap():
    """Replicate test_detects_buy_swap from swap_parser.rs"""
    wallet = "Wallet111111111111111111111111111111111111"
    tx = {
        "transaction": {
            "message": {
                "accountKeys": [wallet, "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4"],
                "instructions": [{"programIdIndex": 1, "accounts": [0], "data": "AA"}]
            },
            "signatures": ["sig111"]
        },
        "meta": {
            "err": None,
            "fee": 5000,
            "preBalances": [1_000_005_000, 0],
            "postBalances": [990_000_000, 0],
            "loadedAddresses": {"writable": [], "readonly": []},
            "preTokenBalances": [],
            "postTokenBalances": [
                {"accountIndex": 2, "mint": "TokenMint111", "owner": wallet,
                 "uiTokenAmount": {"amount": "5000000", "decimals": 6}}
            ]
        },
        "blockTime": 1700000000,
        "slot": 100
    }
    swap = parse_swap_from_transaction(tx, wallet)
    assert swap is not None
    assert swap.direction == "Buy"
    assert swap.input_mint == WSOL_MINT
    assert swap.output_mint == "TokenMint111"
    assert swap.output_amount == 5_000_000
    assert swap.input_amount == 10_000_000
    assert swap.dex == "jupiter_v6"
    print(f"PASS: Buy swap - {swap.dex} {swap.direction} {swap.output_mint}")

def test_sell_swap():
    """Replicate test_detects_sell_swap from swap_parser.rs"""
    wallet = "Wallet111111111111111111111111111111111111"
    tx = {
        "transaction": {
            "message": {
                "accountKeys": [wallet, "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4"],
                "instructions": [{"programIdIndex": 1, "accounts": [0], "data": "AA"}]
            },
            "signatures": ["sig444"]
        },
        "meta": {
            "err": None,
            "fee": 5000,
            "preBalances": [990_000_000, 0],
            "postBalances": [999_995_000, 0],
            "loadedAddresses": {"writable": [], "readonly": []},
            "preTokenBalances": [
                {"accountIndex": 2, "mint": "SellToken111", "owner": wallet,
                 "uiTokenAmount": {"amount": "5000000", "decimals": 6}}
            ],
            "postTokenBalances": []
        },
        "blockTime": 1700000000,
        "slot": 400
    }
    swap = parse_swap_from_transaction(tx, wallet)
    assert swap is not None
    assert swap.direction == "Sell"
    assert swap.input_mint == "SellToken111"
    assert swap.input_amount == 5_000_000
    assert swap.output_mint == WSOL_MINT
    print(f"PASS: Sell swap - {swap.dex} {swap.direction} {swap.input_mint}")

def test_token_to_token():
    """Replicate test_detects_token_to_token_swap"""
    wallet = "Wallet111111111111111111111111111111111111"
    tx = {
        "transaction": {
            "message": {
                "accountKeys": [wallet, "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8"],
                "instructions": [{"programIdIndex": 1, "accounts": [0], "data": "AA"}]
            },
            "signatures": ["sig222"]
        },
        "meta": {
            "err": None,
            "fee": 5000,
            "preBalances": [1_000_000_000, 0],
            "postBalances": [999_995_000, 0],
            "loadedAddresses": {"writable": [], "readonly": []},
            "preTokenBalances": [
                {"accountIndex": 2, "mint": "TokenA111111111111111111111111111111111111",
                 "owner": wallet, "uiTokenAmount": {"amount": "1000000", "decimals": 6}}
            ],
            "postTokenBalances": [
                {"accountIndex": 3, "mint": "TokenB111111111111111111111111111111111111",
                 "owner": wallet, "uiTokenAmount": {"amount": "2000000", "decimals": 6}}
            ]
        },
        "blockTime": 1700000000,
        "slot": 200
    }
    swap = parse_swap_from_transaction(tx, wallet)
    assert swap is not None
    assert swap.input_mint == "TokenA111111111111111111111111111111111111"
    assert swap.output_mint == "TokenB111111111111111111111111111111111111"
    assert swap.dex == "raydium_amm"
    print(f"PASS: Token-to-token - {swap.dex} {swap.input_mint} -> {swap.output_mint}")

def test_non_dex_rejected():
    """Replicate test_rejects_non_dex_balance_change"""
    wallet = "Wallet111111111111111111111111111111111111"
    tx = {
        "transaction": {
            "message": {
                "accountKeys": [wallet, "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"],
                "instructions": [{"programIdIndex": 1, "accounts": [0, 2], "data": "AA"}]
            },
            "signatures": ["sig666"]
        },
        "meta": {
            "err": None,
            "fee": 5000,
            "preBalances": [1_000_000_000, 0],
            "postBalances": [999_995_000, 0],
            "loadedAddresses": {"writable": [], "readonly": []},
            "preTokenBalances": [
                {"accountIndex": 2, "mint": "XferToken111", "owner": wallet,
                 "uiTokenAmount": {"amount": "1000000", "decimals": 6}}
            ],
            "postTokenBalances": [
                {"accountIndex": 2, "mint": "XferToken111", "owner": wallet,
                 "uiTokenAmount": {"amount": "5000000", "decimals": 6}}
            ]
        },
        "blockTime": 1700000000,
        "slot": 600
    }
    swap = parse_swap_from_transaction(tx, wallet)
    assert swap is None, "Non-DEX balance change must NOT be classified as swap"
    print("PASS: Non-DEX transfer correctly rejected")

def test_failed_tx_rejected():
    """Replicate test_ignores_failed_transaction"""
    wallet = "W1"
    tx = {
        "transaction": {
            "message": {
                "accountKeys": [wallet, "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4"],
                "instructions": [{"programIdIndex": 1, "accounts": [0], "data": "AA"}]
            },
            "signatures": ["sig111"]
        },
        "meta": {
            "err": "InstructionError",
            "fee": 5000,
            "preBalances": [1_000_005_000, 0],
            "postBalances": [990_000_000, 0],
            "loadedAddresses": {"writable": [], "readonly": []},
            "preTokenBalances": [],
            "postTokenBalances": [
                {"accountIndex": 2, "mint": "TokenMint111", "owner": wallet,
                 "uiTokenAmount": {"amount": "5000000", "decimals": 6}}
            ]
        },
        "blockTime": 1700000000,
        "slot": 100
    }
    swap = parse_swap_from_transaction(tx, wallet)
    assert swap is None, "Failed transaction must not parse"
    print("PASS: Failed tx correctly rejected")

def test_fifo_completed_trades():
    """Test FIFO lot tracking produces correct completed_trade count."""
    wallet = "Wallet111111111111111111111111111111111111"

    # 3 buys followed by 3 sells of same token -> 3 completed trades
    buy_tx = {
        "transaction": {
            "message": {
                "accountKeys": [wallet, "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4"],
                "instructions": [{"programIdIndex": 1, "accounts": [0], "data": "AA"}]
            },
            "signatures": ["buy1"]
        },
        "meta": {
            "err": None, "fee": 5000,
            "preBalances": [1_000_005_000, 0],
            "postBalances": [990_000_000, 0],
            "loadedAddresses": {"writable": [], "readonly": []},
            "preTokenBalances": [],
            "postTokenBalances": [
                {"accountIndex": 2, "mint": "TESTTOKEN", "owner": wallet,
                 "uiTokenAmount": {"amount": "1000", "decimals": 6}}
            ]
        },
        "blockTime": 1700000000, "slot": 100
    }

    sell_tx = {
        "transaction": {
            "message": {
                "accountKeys": [wallet, "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4"],
                "instructions": [{"programIdIndex": 1, "accounts": [0], "data": "AA"}]
            },
            "signatures": ["sell1"]
        },
        "meta": {
            "err": None, "fee": 5000,
            "preBalances": [990_000_000, 0],
            "postBalances": [999_995_000, 0],
            "loadedAddresses": {"writable": [], "readonly": []},
            "preTokenBalances": [
                {"accountIndex": 2, "mint": "TESTTOKEN", "owner": wallet,
                 "uiTokenAmount": {"amount": "1000", "decimals": 6}}
            ],
            "postTokenBalances": []
        },
        "blockTime": 1700000001, "slot": 101
    }

    acc = WalletAccumulator()
    for _ in range(3):
        swap = parse_swap_from_transaction(buy_tx, wallet)
        acc.record_observation(swap)

    for _ in range(3):
        sell_tx_copy = json.loads(json.dumps(sell_tx))
        swap = parse_swap_from_transaction(sell_tx_copy, wallet)
        acc.record_observation(swap)

    assert acc.buys == 3, f"Expected 3 buys, got {acc.buys}"
    assert acc.sells == 3, f"Expected 3 sells, got {acc.sells}"
    assert len(acc.completed_trades) == 3, f"Expected 3 completed trades, got {len(acc.completed_trades)}"
    print(f"PASS: FIFO tracking - {acc.buys} buys, {acc.sells} sells, {len(acc.completed_trades)} completed trades")

def test_versioned_tx_lookup_table():
    """Replicate test_versioned_tx_with_lookup_table_addresses"""
    wallet = "LookupWallet1111111111111111111111111111111"
    tx = {
        "transaction": {
            "message": {
                "accountKeys": [
                    "FeePayer111111111111111111111111111111111",
                    "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4"
                ],
                "addressTableLookups": [
                    {"accountKey": "LookupTable1111111111111111111111111111111",
                     "writableIndexes": [0], "readonlyIndexes": []}
                ],
                "instructions": [{"programIdIndex": 1, "accounts": [0], "data": "AA"}]
            },
            "signatures": ["sig777"]
        },
        "meta": {
            "err": None, "fee": 5000,
            "preBalances": [1_000_005_000, 0, 1_000_005_000],
            "postBalances": [990_000_000, 0, 990_000_000],
            "loadedAddresses": {
                "writable": [wallet],
                "readonly": []
            },
            "preTokenBalances": [],
            "postTokenBalances": [
                {"accountIndex": 4, "mint": "V0Token111", "owner": wallet,
                 "uiTokenAmount": {"amount": "5000000", "decimals": 6}}
            ]
        },
        "blockTime": 1700000000,
        "slot": 700
    }
    swap = parse_swap_from_transaction(tx, wallet)
    assert swap is not None, "V0 tx with wallet in loadedAddresses must parse"
    assert swap.direction == "Buy"
    assert swap.input_mint == WSOL_MINT
    assert swap.output_mint == "V0Token111"
    assert swap.dex == "jupiter_v6"
    print(f"PASS: Versioned V0 tx - wallet in loadedAddresses parses correctly")


if __name__ == "__main__":
    print("=" * 60)
    print("Testing Python swap parser against Rust semantics")
    print("=" * 60)
    test_real_fixture()
    test_buy_swap()
    test_sell_swap()
    test_token_to_token()
    test_non_dex_rejected()
    test_failed_tx_rejected()
    test_fifo_completed_trades()
    test_versioned_tx_lookup_table()
    print("=" * 60)
    print("ALL TESTS PASSED")
    print(f"COMPLETED_TRADES_THRESHOLD = {COMPLETED_TRADES_THRESHOLD}")
    print("=" * 60)
