//! Extraction of the actual on-chain outcome of a swap transaction.
//!
//! A submitted transaction is never trusted on the basis of a signature
//! response alone: the confirmed transaction metadata is parsed and the real
//! input/output token deltas and fees are derived from pre/post token
//! balances. Anything that cannot be derived is `Unverifiable` and must be
//! reconciled manually rather than assumed.

use serde_json::Value;

pub const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

/// Verified outcome of an on-chain swap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainSwapOutcome {
    pub input_amount: u64,
    pub output_amount: u64,
    pub fee_lamports: u64,
    pub block_time: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SwapOutcome {
    Failed(Value),
    Executed(ChainSwapOutcome),
    /// The transaction confirmed but its economic effect could not be
    /// derived from the metadata. Never treated as success or failure.
    Unverifiable(String),
}

/// Deltas per (account_index, mint) from pre/post token balance arrays.
fn token_deltas(meta: &Value) -> Vec<(u64, String, i128)> {
    let parse = |key: &str| -> Vec<(u64, String, u64)> {
        meta[key]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| {
                        let idx = e["accountIndex"].as_u64()?;
                        let mint = e["mint"].as_str()?.to_string();
                        let amount = e["uiTokenAmount"]["amount"].as_str()?.parse::<u64>().ok()?;
                        Some((idx, mint, amount))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let pre = parse("preTokenBalances");
    let post = parse("postTokenBalances");
    let mut keys: Vec<(u64, String)> = pre.iter().map(|(i, m, _)| (*i, m.clone())).collect();
    keys.extend(post.iter().map(|(i, m, _)| (*i, m.clone())));
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .map(|(idx, mint)| {
            let before = pre
                .iter()
                .find(|(i, m, _)| *i == idx && *m == mint)
                .map(|(_, _, a)| *a)
                .unwrap_or(0) as i128;
            let after = post
                .iter()
                .find(|(i, m, _)| *i == idx && *m == mint)
                .map(|(_, _, a)| *a)
                .unwrap_or(0) as i128;
            (idx, mint, after - before)
        })
        .collect()
}

/// Owner of each token account index, from the balance entries themselves.
fn owner_for_index(meta: &Value) -> Vec<(u64, String)> {
    meta["postTokenBalances"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    let idx = e["accountIndex"].as_u64()?;
                    let owner = e["owner"].as_str()?.to_string();
                    Some((idx, owner))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn native_delta_for_owner(tx: &Value, owner: &str) -> Option<i128> {
    let keys = tx["transaction"]["message"]["accountKeys"].as_array()?;
    let idx = keys
        .iter()
        .position(|k| k["pubkey"].as_str() == Some(owner))?;
    let meta = &tx["meta"];
    let pre = meta["preBalances"].as_array()?.get(idx)?.as_i64()? as i128;
    let post = meta["postBalances"].as_array()?.get(idx)?.as_i64()? as i128;
    Some(post - pre)
}

/// Parses a `getTransaction` response into a verified swap outcome.
///
/// * Wrapped-SOL input legs: the wrapped account is created and closed inside
///   the transaction, so the input amount is taken from the exact-in request
///   (`expected_input`) instead of a token delta.
/// * Wrapped-SOL output legs: derived from the native lamport delta plus the
///   fee (the unwrapped SOL arrives net of fees).
pub fn parse_swap_transaction(
    tx: &Value,
    input_mint: &str,
    output_mint: &str,
    owner: &str,
    expected_input: u64,
) -> SwapOutcome {
    let meta = match tx.get("meta") {
        Some(m) if !m.is_null() => m,
        _ => return SwapOutcome::Unverifiable("missing transaction metadata".into()),
    };
    if let Some(err) = meta.get("err").filter(|e| !e.is_null()) {
        return SwapOutcome::Failed(err.clone());
    }
    let Some(fee_lamports) = meta["fee"].as_u64() else {
        return SwapOutcome::Unverifiable("missing fee metadata".into());
    };
    let owners = owner_for_index(meta);
    let deltas = token_deltas(meta);

    let delta_for = |mint: &str| -> i128 {
        deltas
            .iter()
            .filter(|(idx, m, _)| {
                *m == mint && owners.iter().any(|(oi, o)| oi == idx && o == owner)
            })
            .map(|(_, _, d)| *d)
            .sum()
    };

    let input_amount = if input_mint == WSOL_MINT {
        // Exact-in by construction; the wrap/unwrap pattern nets the token
        // delta to zero, so the requested input is authoritative.
        expected_input
    } else {
        let d = delta_for(input_mint);
        if d <= 0 {
            return SwapOutcome::Unverifiable(format!(
                "no positive owner delta for input mint {input_mint}"
            ));
        }
        match u64::try_from(d) {
            Ok(v) => v,
            Err(_) => return SwapOutcome::Unverifiable("input delta overflow".into()),
        }
    };

    let output_amount = if output_mint == WSOL_MINT {
        match native_delta_for_owner(tx, owner) {
            Some(d) if d > 0 => {
                let gross = d + fee_lamports as i128;
                match u64::try_from(gross) {
                    Ok(v) => v,
                    Err(_) => return SwapOutcome::Unverifiable("output delta overflow".into()),
                }
            }
            _ => {
                return SwapOutcome::Unverifiable(
                    "no positive native delta for wrapped-SOL output".into(),
                )
            }
        }
    } else {
        let d = delta_for(output_mint);
        if d <= 0 {
            return SwapOutcome::Unverifiable(format!(
                "no positive owner delta for output mint {output_mint}"
            ));
        }
        match u64::try_from(d) {
            Ok(v) => v,
            Err(_) => return SwapOutcome::Unverifiable("output delta overflow".into()),
        }
    };

    SwapOutcome::Executed(ChainSwapOutcome {
        input_amount,
        output_amount,
        fee_lamports,
        block_time: tx["blockTime"].as_i64(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const OWNER: &str = "OwnerWallet11111111111111111111111111111111";
    const TOKEN: &str = "TokenMint111111111111111111111111111111111";

    fn tx_json(fee: u64, pre: Value, post: Value, err: Option<Value>, post_native: i64) -> Value {
        json!({
            "transaction": { "message": { "accountKeys": [ {"pubkey": OWNER}, {"pubkey": "Program1111111111111111111111111111111111"} ] } },
            "meta": { "err": err, "fee": fee, "preBalances": [1_000_000, 1_000_000], "postBalances": [post_native, 1_000_000],
                "preTokenBalances": pre, "postTokenBalances": post },
            "blockTime": 1_700_000_000i64
        })
    }
    fn tb(idx: u64, mint: &str, amount: u64, owner: Option<&str>) -> Value {
        let mut e = json!({"accountIndex": idx, "mint": mint, "uiTokenAmount": {"amount": amount.to_string(), "decimals": 6}});
        if let Some(o) = owner {
            e["owner"] = json!(o);
        }
        e
    }

    #[test]
    fn extracts_actual_amounts_and_fees() {
        let tx = tx_json(
            10_000,
            json!([tb(1, TOKEN, 0, Some(OWNER))]),
            json!([tb(1, TOKEN, 5_000_000, Some(OWNER))]),
            None,
            990_000,
        );
        let out = parse_swap_transaction(&tx, WSOL_MINT, TOKEN, OWNER, 1_000_000_000);
        assert_eq!(
            out,
            SwapOutcome::Executed(ChainSwapOutcome {
                input_amount: 1_000_000_000,
                output_amount: 5_000_000,
                fee_lamports: 10_000,
                block_time: Some(1_700_000_000)
            })
        );
    }
    #[test]
    fn wsol_output_comes_from_native_delta_plus_fee() {
        let tx = tx_json(
            10_000,
            json!([tb(1, TOKEN, 5_000_000, Some(OWNER))]),
            json!([tb(1, TOKEN, 0, Some(OWNER))]),
            None,
            1_090_000,
        );
        let out = parse_swap_transaction(&tx, TOKEN, WSOL_MINT, OWNER, 5_000_000);
        match out {
            SwapOutcome::Executed(o) => {
                assert_eq!(o.output_amount, 90_000 + 10_000);
                assert_eq!(o.input_amount, 5_000_000);
            }
            other => panic!("{other:?}"),
        }
    }
    #[test]
    fn onchain_failure_is_distinct_from_unverifiable() {
        let tx = tx_json(
            5_000,
            json!([]),
            json!([]),
            Some(json!("SlippageToleranceExceeded")),
            995_000,
        );
        assert!(matches!(
            parse_swap_transaction(&tx, WSOL_MINT, TOKEN, OWNER, 1),
            SwapOutcome::Failed(_)
        ));
    }
    #[test]
    fn missing_owner_delta_is_unverifiable_not_assumed() {
        let tx = tx_json(
            10_000,
            json!([]),
            json!([tb(
                1,
                TOKEN,
                5_000_000,
                Some("SomeoneElse11111111111111111111111111111111")
            )]),
            None,
            990_000,
        );
        assert!(matches!(
            parse_swap_transaction(&tx, WSOL_MINT, TOKEN, OWNER, 1),
            SwapOutcome::Unverifiable(_)
        ));
        let tx = tx_json(10_000, json!([]), json!([]), None, 990_000);
        assert!(matches!(
            parse_swap_transaction(&tx, WSOL_MINT, TOKEN, OWNER, 1),
            SwapOutcome::Unverifiable(_)
        ));
    }
    #[test]
    fn unowned_balance_entries_without_owner_field_are_refused() {
        let tx = tx_json(
            10_000,
            json!([tb(1, TOKEN, 0, None)]),
            json!([tb(1, TOKEN, 5_000_000, None)]),
            None,
            990_000,
        );
        assert!(matches!(
            parse_swap_transaction(&tx, WSOL_MINT, TOKEN, OWNER, 1),
            SwapOutcome::Unverifiable(_)
        ));
    }
    #[test]
    fn missing_fee_metadata_is_unverifiable() {
        let mut tx = tx_json(10_000, json!([]), json!([]), None, 990_000);
        tx["meta"].as_object_mut().unwrap().remove("fee");
        assert!(matches!(
            parse_swap_transaction(&tx, WSOL_MINT, TOKEN, OWNER, 1),
            SwapOutcome::Unverifiable(_)
        ));
    }
}
