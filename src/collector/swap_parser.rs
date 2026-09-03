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

fn detect_dex(account_keys: &[Value], instructions: &Value) -> String {
    let key_strs: Vec<&str> = account_keys.iter().filter_map(extract_pubkey).collect();
    if let Some(arr) = instructions.as_array() {
        for ix in arr {
            if let Some(idx) = ix["programIdIndex"].as_u64() {
                if let Some(key) = key_strs.get(idx as usize) {
                    for &(program_id, name) in DEX_PROGRAMS {
                        if *key == program_id {
                            return name.to_string();
                        }
                    }
                }
            }
        }
    }
    "unknown".to_string()
}

const DUST_LAMPORTS: u64 = 1_000;

/// Parse a `getTransaction` response and detect a swap for the given wallet.
///
/// Returns `None` if the transaction is not a recognized swap, failed,
/// or does not involve the wallet.
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

    let account_keys = tx["transaction"]["message"]["accountKeys"].as_array()?;
    let wallet_idx = account_keys
        .iter()
        .position(|k| extract_pubkey(k) == Some(wallet))?;

    let pre_sol = meta["preBalances"].as_array()?.get(wallet_idx)?.as_u64()?;
    let post_sol = meta["postBalances"].as_array()?.get(wallet_idx)?.as_u64()?;

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
    let total_sol_spent = -native_sol_change - wsol_delta - fee as i128;

    let instructions = &tx["transaction"]["message"]["instructions"];
    let dex = detect_dex(account_keys, instructions);

    // CASE 1: BUY (SOL → token). Wallet spent SOL and gained at least one token.
    // Real-world swaps often have dust residuals from routing (tiny amounts of
    // intermediate tokens), so we only require at least one significant gain.
    if total_sol_spent > MIN_SWAP_LAMPORTS && !gained.is_empty() {
        // Prefer the largest non-dust gain; fall back to the largest overall.
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

    // CASE 2: SELL (token → SOL). Wallet lost at least one token and received SOL.
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
            return Some(ParsedSwap {
                wallet: wallet.to_string(),
                input_mint: input.0.clone(),
                output_mint: output.0.clone(),
                input_amount: input.1,
                output_amount: output.1,
                input_decimals: input.2,
                output_decimals: output.2,
                direction: if input.0 == WSOL_MINT {
                    SwapDirection::Buy
                } else if output.0 == WSOL_MINT {
                    SwapDirection::Sell
                } else {
                    SwapDirection::Buy
                },
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

const MIN_SWAP_LAMPORTS: i128 = 10_000;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn buy_tx(wallet: &str) -> Value {
        json!({
            "transaction": {
                "message": {
                    "accountKeys": [
                        {"pubkey": wallet, "signer": true, "writable": true},
                        {"pubkey": "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4", "signer": false, "writable": false}
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
                "preBalances": [1_000_005_000, 0],
                "postBalances": [990_000_000, 0],
                "preTokenBalances": [],
                "postTokenBalances": [
                    {"accountIndex": 2, "mint": "TokenMint111", "owner": wallet, "uiTokenAmount": {"amount": "5000000", "decimals": 6, "uiAmount": 5.0, "uiAmountString": "5"}}
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
                        {"pubkey": wallet, "signer": true, "writable": true},
                        {"pubkey": "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8", "signer": false, "writable": false}
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
                "preBalances": [1_000_000_000, 0],
                "postBalances": [999_995_000, 0],
                "preTokenBalances": [
                    {"accountIndex": 2, "mint": "TokenA111111111111111111111111111111111111", "owner": wallet, "uiTokenAmount": {"amount": "1000000", "decimals": 6, "uiAmount": 1.0, "uiAmountString": "1"}}
                ],
                "postTokenBalances": [
                    {"accountIndex": 3, "mint": "TokenB111111111111111111111111111111111111", "owner": wallet, "uiTokenAmount": {"amount": "2000000", "decimals": 6, "uiAmount": 2.0, "uiAmountString": "2"}}
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
                    "accountKeys": [
                        {"pubkey": wallet, "signer": true, "writable": true},
                        {"pubkey": "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4", "signer": false, "writable": false}
                    ],
                    "instructions": [
                        {"programIdIndex": 1, "accounts": [0], "data": "AA"}
                    ]
                },
                "signatures": ["sig333"]
            },
            "meta": {
                "err": null,
                "fee": 5000,
                "preBalances": [1_000_005_000, 0],
                "postBalances": [990_000_000, 0],
                "preTokenBalances": [],
                "postTokenBalances": [
                    {"accountIndex": 2, "mint": "MainToken1111111111111111111111111111111", "owner": wallet, "uiTokenAmount": {"amount": "5000000", "decimals": 6, "uiAmount": 5.0, "uiAmountString": "5"}},
                    {"accountIndex": 3, "mint": "DustToken111111111111111111111111111111111", "owner": wallet, "uiTokenAmount": {"amount": "100", "decimals": 6, "uiAmount": 0.0001, "uiAmountString": "0.0001"}}
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
                    "accountKeys": [
                        {"pubkey": wallet, "signer": true, "writable": true},
                        {"pubkey": "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4", "signer": false, "writable": false}
                    ],
                    "instructions": [
                        {"programIdIndex": 1, "accounts": [0], "data": "AA"}
                    ]
                },
                "signatures": ["sig444"]
            },
            "meta": {
                "err": null,
                "fee": 5000,
                "preBalances": [990_000_000, 0],
                "postBalances": [999_995_000, 0],
                "preTokenBalances": [
                    {"accountIndex": 2, "mint": "SellToken111111111111111111111111111111111", "owner": wallet, "uiTokenAmount": {"amount": "5000000", "decimals": 6, "uiAmount": 5.0, "uiAmountString": "5"}}
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
                    "accountKeys": [
                        {"pubkey": wallet, "signer": true, "writable": true},
                        {"pubkey": "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4", "signer": false, "writable": false}
                    ],
                    "instructions": [
                        {"programIdIndex": 1, "accounts": [0], "data": "AA"}
                    ]
                },
                "signatures": ["sig555"]
            },
            "meta": {
                "err": null,
                "fee": 5000,
                "preBalances": [990_000_000, 0],
                "postBalances": [999_995_000, 0],
                "preTokenBalances": [
                    {"accountIndex": 2, "mint": "SellToken111111111111111111111111111111111", "owner": wallet, "uiTokenAmount": {"amount": "5000000", "decimals": 6, "uiAmount": 5.0, "uiAmountString": "5"}},
                    {"accountIndex": 3, "mint": "DustToken111111111111111111111111111111111", "owner": wallet, "uiTokenAmount": {"amount": "50", "decimals": 6, "uiAmount": 0.00005, "uiAmountString": "0.00005"}}
                ],
                "postTokenBalances": [
                    {"accountIndex": 3, "mint": "DustToken111111111111111111111111111111111", "owner": wallet, "uiTokenAmount": {"amount": "150", "decimals": 6, "uiAmount": 0.00015, "uiAmountString": "0.00015"}}
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
}
