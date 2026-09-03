use serde_json::Value;
use std::collections::HashMap;

pub const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

const DEX_PROGRAMS: &[(&str, &str)] = &[
    ("JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4", "jupiter_v6"),
    ("JUP4Fb2cqiRUcaTHdrPC8h2gNsA2ETXiPDD33WcGuJB", "jupiter_v4"),
    ("JUP3jqKShLTC4TXbKQ9sRMSMgGKHGSuR3wHkJz2Bjqp", "jupiter_v3"),
    (
        "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8",
        "raydium_amm",
    ),
    (
        "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK",
        "raydium_clmm",
    ),
    (
        "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C",
        "raydium_cpmm",
    ),
    (
        "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",
        "orca_whirlpool",
    ),
    ("PhoeNiXZ8ByJGLkxNfZRnkUfjvmuYqLR89jjFHGqdXY", "phoenix"),
    (
        "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSK9PK88t2jzdc",
        "meteora_dlmm",
    ),
    (
        "Eo7WjKq67rjJQSZxS6z3YkapzS3LLjgeF31Rr9oFk7JE",
        "meteora_dyn_amm",
    ),
    ("SSwapUtyTFBdGF1rCvjz4wp4Y8tE1dPi54Fo8SPauX6", "saber"),
    ("SwaPpA9LAaLfeLi3a68M4DjnLqgtticKg6CnyNwgAC8", "saber"),
    ("jupiter6ooCZ...ignored", "ignore"), // placeholder so length > 0
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwapDirection {
    Buy,
    Sell,
}

#[derive(Debug, Clone)]
pub struct ParsedSwap {
    pub wallet: String,
    pub input_mint: String,
    pub output_mint: String,
    pub input_amount: u64,
    pub output_amount: u64,
    pub input_decimals: u8,
    pub output_decimals: u8,
    pub direction: SwapDirection,
    pub fee_lamports: u64,
    pub dex: String,
    pub slot: u64,
    pub block_time: i64,
    pub signature: String,
}

fn extract_pubkey(key: &Value) -> Option<&str> {
    if let Some(s) = key.as_str() {
        return Some(s);
    }
    key["pubkey"].as_str()
}

fn parse_token_balances_for_owner(balances: &Value, wallet: &str) -> HashMap<String, (u64, u8)> {
    let mut result = HashMap::new();
    if let Some(arr) = balances.as_array() {
        for entry in arr {
            if entry["owner"].as_str() == Some(wallet) {
                let mint = match entry["mint"].as_str() {
                    Some(m) => m.to_string(),
                    None => continue,
                };
                let amount = entry["uiTokenAmount"]["amount"]
                    .as_str()
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);
                let decimals = entry["uiTokenAmount"]["decimals"].as_u64().unwrap_or(0) as u8;
                let e = result.entry(mint).or_insert((0u64, decimals));
                e.0 = e.0.saturating_add(amount);
            }
        }
    }
    result
}

/// Build the full list of account keys (static + loaded lookup-table addresses)
/// in the same order that `preBalances`/`postBalances` use:
/// 1. static keys from `accountKeys`
/// 2. `meta.loadedAddresses.writable` (resolved by RPC node)
/// 3. `meta.loadedAddresses.readonly` (resolved by RPC node)
fn build_all_account_keys(tx: &Value) -> Option<Vec<String>> {
    let static_keys = tx["transaction"]["message"]["accountKeys"].as_array()?;
    let mut keys: Vec<String> = static_keys
        .iter()
        .filter_map(extract_pubkey)
        .map(|s| s.to_string())
        .collect();

    if let Some(loaded) = tx["meta"]["loadedAddresses"].as_object() {
        if let Some(writable) = loaded["writable"].as_array() {
            for k in writable {
                if let Some(s) = k.as_str() {
                    keys.push(s.to_string());
                }
            }
        }
        if let Some(readonly) = loaded["readonly"].as_array() {
            for k in readonly {
                if let Some(s) = k.as_str() {
                    keys.push(s.to_string());
                }
            }
        }
    } else if let Some(lookups) = tx["transaction"]["message"]["addressTableLookups"].as_array() {
        // Fallback: very old RPC nodes may not return loadedAddresses and instead
        // embed the lookup-table references inside the message. We cannot resolve
        // these without the table contents, so skip silently rather than fabricate.
        for _lookup in lookups {
            // No-op; this branch documents that we intentionally don't try to
            // resolve lookup tables from the message alone.
        }
    }

    Some(keys)
}

/// Scan top-level + inner instructions for any program that matches a known
/// DEX. Returns the first matching DEX name, or "unknown" if none match.
fn detect_dex(tx: &Value, all_keys: &[String]) -> String {
    let msg = &tx["transaction"]["message"];
    let top = msg["instructions"].as_array();
    let inner_blocks = tx["meta"]["innerInstructions"].as_array();

    let check = |program_idx: usize| {
        all_keys
            .get(program_idx)
            .and_then(|k| {
                DEX_PROGRAMS
                    .iter()
                    .find(|(id, _)| *id == k.as_str())
                    .map(|(_, name)| name.to_string())
            })
            .unwrap_or_default()
    };

    if let Some(ixs) = top {
        for ix in ixs {
            if let Some(pid) = ix["programIdIndex"].as_u64() {
                let d = check(pid as usize);
                if !d.is_empty() && d != "ignore" {
                    return d;
                }
            }
        }
    }
    if let Some(blocks) = inner_blocks {
        for block in blocks {
            if let Some(ixs) = block["instructions"].as_array() {
                for ix in ixs {
                    if let Some(pid) = ix["programIdIndex"].as_u64() {
                        let d = check(pid as usize);
                        if !d.is_empty() && d != "ignore" {
                            return d;
                        }
                    }
                }
            }
        }
    }
    "unknown".to_string()
}

const DUST_LAMPORTS: u64 = 1_000;
const MIN_SWAP_LAMPORTS: i128 = 1_000;

/// Parse a `getTransaction` response and detect a swap for the given wallet.
///
/// Returns `None` if the transaction is not a recognized swap, failed,
/// does not involve the wallet, or does not invoke a known DEX program.
pub fn parse_swap_from_transaction(tx: &Value, wallet: &str) -> Option<ParsedSwap> {
    let meta = tx.get("meta")?;
    if meta.get("err").filter(|e| !e.is_null()).is_some() {
        return None;
    }

    let signature = tx["transaction"]["signatures"]
        .as_array()
        .and_then(|s| s.first())
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let slot = tx["slot"].as_u64().unwrap_or(0);
    let block_time = tx["blockTime"].as_i64().unwrap_or(0);
    let fee = meta["fee"].as_u64().unwrap_or(0);

    let all_account_keys = build_all_account_keys(tx)?;
    let wallet_idx = all_account_keys.iter().position(|k| k == wallet)?;

    let pre_balances = meta["preBalances"].as_array()?;
    let post_balances = meta["postBalances"].as_array()?;
    if pre_balances.len() != post_balances.len()
        || wallet_idx >= pre_balances.len()
        || wallet_idx >= post_balances.len()
    {
        return None;
    }

    let pre_sol = pre_balances[wallet_idx].as_u64()?;
    let post_sol = post_balances[wallet_idx].as_u64()?;

    let pre_tokens = parse_token_balances_for_owner(&meta["preTokenBalances"], wallet);
    let post_tokens = parse_token_balances_for_owner(&meta["postTokenBalances"], wallet);

    let mut all_mints: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for m in pre_tokens.keys().chain(post_tokens.keys()) {
        if seen.insert(m.clone()) {
            all_mints.push(m.clone());
        }
    }

    let mut wsol_delta: i128 = 0;
    let mut gained: Vec<(String, u64, u8)> = Vec::new();
    let mut lost: Vec<(String, u64, u8)> = Vec::new();

    for mint in &all_mints {
        let pre_amt = pre_tokens.get(mint).map(|(a, _)| *a).unwrap_or(0);
        let post_amt = post_tokens.get(mint).map(|(a, _)| *a).unwrap_or(0);
        let decimals = post_tokens
            .get(mint)
            .or_else(|| pre_tokens.get(mint))
            .map(|(_, d)| *d)
            .unwrap_or(0);

        let delta = post_amt as i128 - pre_amt as i128;
        if mint == WSOL_MINT {
            wsol_delta = delta;
        } else if delta > 0 {
            gained.push((mint.clone(), delta as u64, decimals));
        } else if delta < 0 {
            lost.push((mint.clone(), (-delta) as u64, decimals));
        }
    }

    let native_sol_change = post_sol as i128 - pre_sol as i128;
    // Total SOL spent from the wallet = native SOL change that was NOT
    // attributable to wsol wrapping and NOT the network fee.
    // native_sol_change is negative for SOL out; positive for SOL in.
    let total_sol_spent = -native_sol_change - wsol_delta - fee as i128;

    let dex = detect_dex(tx, &all_account_keys);

    // Require a known DEX. Token transfers, ATA creates, or non-DEX
    // programs must not be classified as swaps even if balances change.
    if dex == "unknown" {
        return None;
    }

    // CASE 1: BUY (SOL/WSOL → token). Wallet spent SOL and gained at least one token.
    if total_sol_spent > MIN_SWAP_LAMPORTS && !gained.is_empty() {
        let primary = gained
            .iter()
            .filter(|(_, amt, _)| *amt >= DUST_LAMPORTS)
            .max_by_key(|(_, amt, _)| *amt)
            .or_else(|| gained.iter().max_by_key(|(_, amt, _)| *amt));
        if let Some((mint, amount, decimals)) = primary.cloned() {
            return Some(ParsedSwap {
                wallet: wallet.to_string(),
                input_mint: WSOL_MINT.to_string(),
                output_mint: mint,
                input_amount: total_sol_spent as u64,
                output_amount: amount,
                input_decimals: 9,
                output_decimals: decimals,
                direction: SwapDirection::Buy,
                fee_lamports: fee,
                dex,
                slot,
                block_time,
                signature,
            });
        }
    }

    // CASE 2: SELL (token → SOL/WSOL). Wallet lost at least one token and received SOL.
    if total_sol_spent < -MIN_SWAP_LAMPORTS && !lost.is_empty() {
        let primary = lost
            .iter()
            .filter(|(_, amt, _)| *amt >= DUST_LAMPORTS)
            .max_by_key(|(_, amt, _)| *amt)
            .or_else(|| lost.iter().max_by_key(|(_, amt, _)| *amt));
        if let Some((mint, amount, decimals)) = primary.cloned() {
            return Some(ParsedSwap {
                wallet: wallet.to_string(),
                input_mint: mint,
                output_mint: WSOL_MINT.to_string(),
                input_amount: amount,
                output_amount: (-total_sol_spent) as u64,
                input_decimals: decimals,
                output_decimals: 9,
                direction: SwapDirection::Sell,
                fee_lamports: fee,
                dex,
                slot,
                block_time,
                signature,
            });
        }
    }

    // CASE 3: Token-to-token swap (negligible SOL change).
    if !lost.is_empty() && !gained.is_empty() && total_sol_spent.abs() < MIN_SWAP_LAMPORTS {
        let primary_lost = lost
            .iter()
            .filter(|(_, amt, _)| *amt >= DUST_LAMPORTS)
            .max_by_key(|(_, amt, _)| *amt)
            .or_else(|| lost.iter().max_by_key(|(_, amt, _)| *amt));
        let primary_gained = gained
            .iter()
            .filter(|(_, amt, _)| *amt >= DUST_LAMPORTS)
            .max_by_key(|(_, amt, _)| *amt)
            .or_else(|| gained.iter().max_by_key(|(_, amt, _)| *amt));
        if let (Some(input), Some(output)) = (primary_lost, primary_gained) {
            let input = input.clone();
            let output = output.clone();
            let direction = if input.0 == WSOL_MINT {
                SwapDirection::Buy
            } else if output.0 == WSOL_MINT {
                SwapDirection::Sell
            } else {
                SwapDirection::Buy
            };
            return Some(ParsedSwap {
                wallet: wallet.to_string(),
                input_mint: input.0.clone(),
                output_mint: output.0.clone(),
                input_amount: input.1,
                output_amount: output.1,
                input_decimals: input.2,
                output_decimals: output.2,
                direction,
                fee_lamports: fee,
                dex,
                slot,
                block_time,
                signature,
            });
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn buy_tx(wallet: &str) -> Value {
        json!({
            "transaction": {
                "message": {
                    "accountKeys": [
                        wallet,
                        "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4"
                    ],
                    "instructions": [
                        {"programIdIndex": 1, "accounts": [0], "data": "AA"}
                    ]
                },
                "signatures": ["sig111"]
            },
            "meta": {
                "err": null,
                "fee": 5000,
                "preBalances": [1_000_005_000u64, 0],
                "postBalances": [990_000_000u64, 0],
                "loadedAddresses": {"writable": [], "readonly": []},
                "preTokenBalances": [],
                "postTokenBalances": [
                    {"accountIndex": 2, "mint": "TokenMint111", "owner": wallet, "uiTokenAmount": {"amount": "5000000", "decimals": 6}}
                ]
            },
            "blockTime": 1700000000i64,
            "slot": 100
        })
    }

    #[test]
    fn detects_buy_swap() {
        let wallet = "Wallet111111111111111111111111111111111111";
        let swap = parse_swap_from_transaction(&buy_tx(wallet), wallet).unwrap();
        assert_eq!(swap.direction, SwapDirection::Buy);
        assert_eq!(swap.input_mint, WSOL_MINT);
        assert_eq!(swap.output_mint, "TokenMint111");
        assert_eq!(swap.output_amount, 5_000_000);
        assert_eq!(swap.input_amount, 10_000_000);
        assert_eq!(swap.dex, "jupiter_v6");
    }

    #[test]
    fn ignores_failed_transaction() {
        let mut tx = buy_tx("W1");
        tx["meta"]["err"] = json!("InstructionError");
        assert!(parse_swap_from_transaction(&tx, "W1").is_none());
    }

    #[test]
    fn ignores_unrelated_wallet() {
        let swap = parse_swap_from_transaction(&buy_tx("Wallet111"), "OtherWallet111");
        assert!(swap.is_none());
    }

    #[test]
    fn detects_token_to_token_swap() {
        let wallet = "Wallet111111111111111111111111111111111111";
        let tx = json!({
            "transaction": {
                "message": {
                    "accountKeys": [
                        wallet,
                        "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8"
                    ],
                    "instructions": [
                        {"programIdIndex": 1, "accounts": [0], "data": "AA"}
                    ]
                },
                "signatures": ["sig222"]
            },
            "meta": {
                "err": null,
                "fee": 5000,
                "preBalances": [1_000_000_000u64, 0],
                "postBalances": [999_995_000u64, 0],
                "loadedAddresses": {"writable": [], "readonly": []},
                "preTokenBalances": [
                    {"accountIndex": 2, "mint": "TokenA111111111111111111111111111111111111", "owner": wallet, "uiTokenAmount": {"amount": "1000000", "decimals": 6}}
                ],
                "postTokenBalances": [
                    {"accountIndex": 3, "mint": "TokenB111111111111111111111111111111111111", "owner": wallet, "uiTokenAmount": {"amount": "2000000", "decimals": 6}}
                ]
            },
            "blockTime": 1700000000i64,
            "slot": 200
        });
        let swap = parse_swap_from_transaction(&tx, wallet).unwrap();
        assert_eq!(
            swap.input_mint,
            "TokenA111111111111111111111111111111111111"
        );
        assert_eq!(
            swap.output_mint,
            "TokenB111111111111111111111111111111111111"
        );
        assert_eq!(swap.dex, "raydium_amm");
    }

    #[test]
    fn detects_buy_with_dust_residual() {
        let wallet = "Wallet111111111111111111111111111111111111";
        let tx = json!({
            "transaction": {
                "message": {
                    "accountKeys": [wallet, "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4"],
                    "instructions": [{"programIdIndex": 1, "accounts": [0], "data": "AA"}]
                },
                "signatures": ["sig333"]
            },
            "meta": {
                "err": null,
                "fee": 5000,
                "preBalances": [1_000_005_000u64, 0],
                "postBalances": [990_000_000u64, 0],
                "loadedAddresses": {"writable": [], "readonly": []},
                "preTokenBalances": [],
                "postTokenBalances": [
                    {"accountIndex": 2, "mint": "MainToken1111111111111111111111111111111", "owner": wallet, "uiTokenAmount": {"amount": "5000000", "decimals": 6}},
                    {"accountIndex": 3, "mint": "DustToken111111111111111111111111111111111", "owner": wallet, "uiTokenAmount": {"amount": "100", "decimals": 6}}
                ]
            },
            "blockTime": 1700000000i64,
            "slot": 300
        });
        let swap = parse_swap_from_transaction(&tx, wallet).unwrap();
        assert_eq!(swap.direction, SwapDirection::Buy);
        assert_eq!(swap.output_mint, "MainToken1111111111111111111111111111111");
        assert_eq!(swap.output_amount, 5_000_000);
    }

    #[test]
    fn detects_sell_swap() {
        let wallet = "Wallet111111111111111111111111111111111111";
        let tx = json!({
            "transaction": {
                "message": {
                    "accountKeys": [wallet, "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4"],
                    "instructions": [{"programIdIndex": 1, "accounts": [0], "data": "AA"}]
                },
                "signatures": ["sig444"]
            },
            "meta": {
                "err": null,
                "fee": 5000,
                "preBalances": [990_000_000u64, 0],
                "postBalances": [999_995_000u64, 0],
                "loadedAddresses": {"writable": [], "readonly": []},
                "preTokenBalances": [
                    {"accountIndex": 2, "mint": "SellToken111111111111111111111111111111111", "owner": wallet, "uiTokenAmount": {"amount": "5000000", "decimals": 6}}
                ],
                "postTokenBalances": []
            },
            "blockTime": 1700000000i64,
            "slot": 400
        });
        let swap = parse_swap_from_transaction(&tx, wallet).unwrap();
        assert_eq!(swap.direction, SwapDirection::Sell);
        assert_eq!(
            swap.input_mint,
            "SellToken111111111111111111111111111111111"
        );
        assert_eq!(swap.input_amount, 5_000_000);
        assert_eq!(swap.output_mint, WSOL_MINT);
    }

    #[test]
    fn detects_sell_with_dust_residual() {
        let wallet = "Wallet111111111111111111111111111111111111";
        let tx = json!({
            "transaction": {
                "message": {
                    "accountKeys": [wallet, "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4"],
                    "instructions": [{"programIdIndex": 1, "accounts": [0], "data": "AA"}]
                },
                "signatures": ["sig555"]
            },
            "meta": {
                "err": null,
                "fee": 5000,
                "preBalances": [990_000_000u64, 0],
                "postBalances": [999_995_000u64, 0],
                "loadedAddresses": {"writable": [], "readonly": []},
                "preTokenBalances": [
                    {"accountIndex": 2, "mint": "SellToken111111111111111111111111111111111", "owner": wallet, "uiTokenAmount": {"amount": "5000000", "decimals": 6}},
                    {"accountIndex": 3, "mint": "DustToken111111111111111111111111111111111", "owner": wallet, "uiTokenAmount": {"amount": "50", "decimals": 6}}
                ],
                "postTokenBalances": [
                    {"accountIndex": 3, "mint": "DustToken111111111111111111111111111111111", "owner": wallet, "uiTokenAmount": {"amount": "150", "decimals": 6}}
                ]
            },
            "blockTime": 1700000000i64,
            "slot": 500
        });
        let swap = parse_swap_from_transaction(&tx, wallet).unwrap();
        assert_eq!(swap.direction, SwapDirection::Sell);
        assert_eq!(
            swap.input_mint,
            "SellToken111111111111111111111111111111111"
        );
        assert_eq!(swap.input_amount, 5_000_000);
    }

    #[test]
    fn rejects_non_dex_balance_change() {
        // A pure SPL transfer between two accounts must NOT be classified as a swap
        // even though it changes the wallet's token balance.
        let wallet = "Wallet111111111111111111111111111111111111";
        let tx = json!({
            "transaction": {
                "message": {
                    "accountKeys": [
                        wallet,
                        "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
                    ],
                    "instructions": [
                        {"programIdIndex": 1, "accounts": [0, 2], "data": "AA"}
                    ]
                },
                "signatures": ["sig666"]
            },
            "meta": {
                "err": null,
                "fee": 5000,
                "preBalances": [1_000_000_000u64, 0],
                "postBalances": [999_995_000u64, 0],
                "loadedAddresses": {"writable": [], "readonly": []},
                "preTokenBalances": [
                    {"accountIndex": 2, "mint": "XferToken11111111111111111111111111111111111", "owner": wallet, "uiTokenAmount": {"amount": "1000000", "decimals": 6}}
                ],
                "postTokenBalances": [
                    {"accountIndex": 2, "mint": "XferToken11111111111111111111111111111111111", "owner": wallet, "uiTokenAmount": {"amount": "5000000", "decimals": 6}}
                ]
            },
            "blockTime": 1700000000i64,
            "slot": 600
        });
        assert!(parse_swap_from_transaction(&tx, wallet).is_none());
    }

    #[test]
    fn versioned_tx_with_lookup_table_addresses() {
        // V0 transaction where the wallet itself only appears in loadedAddresses.
        let wallet = "LookupWallet1111111111111111111111111111111";
        let tx = json!({
            "transaction": {
                "message": {
                    "accountKeys": [
                        "FeePayer111111111111111111111111111111111",
                        "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4"
                    ],
                    "addressTableLookups": [
                        {"accountKey": "LookupTable1111111111111111111111111111111", "writableIndexes": [0], "readonlyIndexes": []}
                    ],
                    "instructions": [{"programIdIndex": 1, "accounts": [0], "data": "AA"}]
                },
                "signatures": ["sig777"]
            },
            "meta": {
                "err": null,
                "fee": 5000,
                "preBalances": [
                    1_000_005_000u64,
                    0,
                    // index 2 = loaded writable = our wallet
                    1_000_005_000u64
                ],
                "postBalances": [
                    990_000_000u64,
                    0,
                    990_000_000u64
                ],
                "loadedAddresses": {
                    "writable": [wallet],
                    "readonly": []
                },
                "preTokenBalances": [],
                "postTokenBalances": [
                    {"accountIndex": 4, "mint": "V0Token11111111111111111111111111111111", "owner": wallet, "uiTokenAmount": {"amount": "5000000", "decimals": 6}}
                ]
            },
            "blockTime": 1700000000i64,
            "slot": 700
        });
        let swap = parse_swap_from_transaction(&tx, wallet);
        assert!(
            swap.is_some(),
            "V0 transaction with wallet in loadedAddresses should parse"
        );
        let swap = swap.unwrap();
        assert_eq!(swap.direction, SwapDirection::Buy);
        assert_eq!(swap.input_mint, WSOL_MINT);
        assert_eq!(swap.output_mint, "V0Token11111111111111111111111111111111");
        assert_eq!(swap.dex, "jupiter_v6");
    }

    #[test]
    fn detects_buy_with_multiple_token_gains() {
        // Jupiter may route through intermediates. Verify the parser picks
        // the largest non-dust gain as the primary output.
        let wallet = "Wallet111111111111111111111111111111111111";
        let tx = json!({
            "transaction": {
                "message": {
                    "accountKeys": [wallet, "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4"],
                    "instructions": [{"programIdIndex": 1, "accounts": [0], "data": "AA"}]
                },
                "signatures": ["sig888"]
            },
            "meta": {
                "err": null,
                "fee": 5000,
                "preBalances": [1_000_005_000u64, 0],
                "postBalances": [990_000_000u64, 0],
                "loadedAddresses": {"writable": [], "readonly": []},
                "preTokenBalances": [],
                "postTokenBalances": [
                    {"accountIndex": 2, "mint": "RouteInterim111111111111111111111111111111", "owner": wallet, "uiTokenAmount": {"amount": "50", "decimals": 6}},
                    {"accountIndex": 3, "mint": "RealOut11111111111111111111111111111111111111", "owner": wallet, "uiTokenAmount": {"amount": "9000000", "decimals": 6}}
                ]
            },
            "blockTime": 1700000000i64,
            "slot": 800
        });
        let swap = parse_swap_from_transaction(&tx, wallet).unwrap();
        assert_eq!(
            swap.output_mint,
            "RealOut11111111111111111111111111111111111111"
        );
        assert_eq!(swap.output_amount, 9_000_000);
    }

    #[test]
    fn detects_sell_with_multiple_token_losses() {
        let wallet = "Wallet111111111111111111111111111111111111";
        let tx = json!({
            "transaction": {
                "message": {
                    "accountKeys": [wallet, "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4"],
                    "instructions": [{"programIdIndex": 1, "accounts": [0], "data": "AA"}]
                },
                "signatures": ["sig999"]
            },
            "meta": {
                "err": null,
                "fee": 5000,
                "preBalances": [990_000_000u64, 0],
                "postBalances": [999_995_000u64, 0],
                "loadedAddresses": {"writable": [], "readonly": []},
                "preTokenBalances": [
                    {"accountIndex": 2, "mint": "Dust1111111111111111111111111111111111111", "owner": wallet, "uiTokenAmount": {"amount": "100", "decimals": 6}},
                    {"accountIndex": 3, "mint": "RealSell11111111111111111111111111111111111", "owner": wallet, "uiTokenAmount": {"amount": "5000000", "decimals": 6}}
                ],
                "postTokenBalances": [
                    {"accountIndex": 2, "mint": "Dust1111111111111111111111111111111111111", "owner": wallet, "uiTokenAmount": {"amount": "100", "decimals": 6}}
                ]
            },
            "blockTime": 1700000000i64,
            "slot": 900
        });
        let swap = parse_swap_from_transaction(&tx, wallet).unwrap();
        assert_eq!(swap.direction, SwapDirection::Sell);
        assert_eq!(
            swap.input_mint,
            "RealSell11111111111111111111111111111111111"
        );
        assert_eq!(swap.input_amount, 5_000_000);
    }

    #[test]
    fn raydium_clmm_buy_detected() {
        let wallet = "Wallet111111111111111111111111111111111111";
        let tx = json!({
            "transaction": {
                "message": {
                    "accountKeys": [wallet, "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK"],
                    "instructions": [{"programIdIndex": 1, "accounts": [0], "data": "AA"}]
                },
                "signatures": ["sigAAA"]
            },
            "meta": {
                "err": null,
                "fee": 5000,
                "preBalances": [1_000_005_000u64, 0],
                "postBalances": [990_000_000u64, 0],
                "loadedAddresses": {"writable": [], "readonly": []},
                "preTokenBalances": [],
                "postTokenBalances": [
                    {"accountIndex": 2, "mint": "ClmmToken11111111111111111111111111111111", "owner": wallet, "uiTokenAmount": {"amount": "5000000", "decimals": 6}}
                ]
            },
            "blockTime": 1700000000i64,
            "slot": 1000
        });
        let swap = parse_swap_from_transaction(&tx, wallet).unwrap();
        assert_eq!(swap.dex, "raydium_clmm");
    }

    #[test]
    fn orca_whirlpool_sell_detected() {
        let wallet = "Wallet111111111111111111111111111111111111";
        let tx = json!({
            "transaction": {
                "message": {
                    "accountKeys": [wallet, "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc"],
                    "instructions": [{"programIdIndex": 1, "accounts": [0], "data": "AA"}]
                },
                "signatures": ["sigBBB"]
            },
            "meta": {
                "err": null,
                "fee": 5000,
                "preBalances": [990_000_000u64, 0],
                "postBalances": [999_995_000u64, 0],
                "loadedAddresses": {"writable": [], "readonly": []},
                "preTokenBalances": [
                    {"accountIndex": 2, "mint": "OrcaToken1111111111111111111111111111111111", "owner": wallet, "uiTokenAmount": {"amount": "5000000", "decimals": 6}}
                ],
                "postTokenBalances": []
            },
            "blockTime": 1700000000i64,
            "slot": 1100
        });
        let swap = parse_swap_from_transaction(&tx, wallet).unwrap();
        assert_eq!(swap.dex, "orca_whirlpool");
        assert_eq!(swap.direction, SwapDirection::Sell);
    }

    #[test]
    fn wsol_wrap_unwrap_not_detected_as_swap() {
        // A pure wsol wrap (SOL → WSOL with no other token change) should
        // not classify as a swap because no DEX program was invoked.
        let wallet = "Wallet111111111111111111111111111111111111";
        let tx = json!({
            "transaction": {
                "message": {
                    "accountKeys": [
                        wallet,
                        "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
                        WSOL_MINT
                    ],
                    "instructions": [{"programIdIndex": 1, "accounts": [0], "data": "AA"}]
                },
                "signatures": ["sigCCC"]
            },
            "meta": {
                "err": null,
                "fee": 5000,
                "preBalances": [10_005_000u64, 0, 0],
                "postBalances": [5_000u64, 0, 0],
                "loadedAddresses": {"writable": [], "readonly": []},
                "preTokenBalances": [],
                "postTokenBalances": [
                    {"accountIndex": 3, "mint": WSOL_MINT, "owner": wallet, "uiTokenAmount": {"amount": "10000000000", "decimals": 9}}
                ]
            },
            "blockTime": 1700000000i64,
            "slot": 1200
        });
        assert!(
            parse_swap_from_transaction(&tx, wallet).is_none(),
            "pure WSOL wrap without DEX must not be classified as a swap"
        );
    }

    #[test]
    fn real_mainnet_jupiter_v6_swap_fixture() {
        // Real transaction captured from Solana mainnet (slot ~444067218,
        // block 1788468803). Wallet 8o29iwRGc3XDe9139HBWsh5ykqHL3K8rpQ2YQGSF9uLo
        // spent ~400 SOL and received ~41.83 USDT via Jupiter v6 in a real
        // versioned transaction with 12 loaded writable + 13 loaded readonly
        // addresses.  The RPC provider returns `accountKeys` as plain
        // strings (legacy encoding), not as `{pubkey, ...}` objects.
        let fixture_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("data/fixtures/real_jupiter_v6_swap.json");
        let raw = std::fs::read_to_string(&fixture_path)
            .unwrap_or_else(|e| panic!("missing fixture {}: {e}", fixture_path.display()));
        let tx: Value = serde_json::from_str(&raw).expect("fixture must be valid JSON");
        let wallet = "8o29iwRGc3XDe9139HBWsh5ykqHL3K8rpQ2YQGSF9uLo";
        let swap = parse_swap_from_transaction(&tx, wallet)
            .expect("real mainnet Jupiter v6 swap must parse");
        assert_eq!(swap.wallet, wallet);
        assert_eq!(swap.dex, "jupiter_v6");
        // The wallet spent SOL and received USDT: this is a Buy
        // (input = SOL, output = USDT).
        assert_eq!(swap.direction, SwapDirection::Buy);
        assert_eq!(swap.input_mint, WSOL_MINT);
        assert_eq!(
            swap.output_mint,
            "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB"
        );
        // Wallet USDT balance: 100_552_404 -> 142_378_708, delta = 41_826_304 atomic.
        assert_eq!(swap.output_amount, 41_826_304);
        // SOL spent ~0.4 SOL minus fee; allow small slack for priority fee.
        // Wallet SOL: 457_395_783 -> 57_230_783 lamports (~0.4 SOL).
        assert!(swap.input_amount > 380_000_000);
        assert!(swap.input_amount < 410_000_000);
    }
}
