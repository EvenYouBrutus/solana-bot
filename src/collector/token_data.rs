use crate::data::rpc::RpcPool;
use crate::domain::market::MarketSnapshot;
use crate::domain::token::TokenSafety;
use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
/// Page size when walking a mint account's signature history backward.
const AGE_PAGE_LIMIT: u32 = 200;
/// Hard bound on RPC pages spent proving a token's age. When the bound is
/// reached without proof the age is treated as unverifiable and the candidate
/// is rejected rather than guessed.
const MAX_AGE_PAGES: u32 = 10;

/// Estimate pool liquidity from a real quote's price impact: a trade of size
/// X causing `p` basis points of impact implies liquidity ≈ X / (p/10000).
/// A zero (sub-bps-rounded) impact means the trade is negligible relative to
/// pool depth and we cannot estimate liquidity. We return zero, which will
/// cause the candidate to be rejected by min_liquidity_usd — the correct
/// fail-closed behavior.
pub fn estimate_liquidity_usd(trade_size_usd: Decimal, price_impact_bps: u32) -> Decimal {
    if price_impact_bps > 0 {
        (trade_size_usd * dec!(10000) / Decimal::from(price_impact_bps)).round_dp(2)
    } else {
        Decimal::ZERO
    }
}

/// Fetch real token safety data from the Solana chain via RPC.
///
/// Returns `Err` if any required RPC call fails. Returns `Ok(None)` if the
/// mint account does not exist or its age cannot be verified.
pub async fn fetch_token_safety(
    rpc: &RpcPool,
    mint: &str,
    min_token_age_secs: i64,
) -> Result<Option<TokenSafety>, anyhow::Error> {
    let mint_info = rpc
        .mint_account_info(mint)
        .await
        .map_err(|e| anyhow::anyhow!("mint_account_info RPC failed: {e}"))?;

    let info = match mint_info {
        Some(i) => i,
        None => return Ok(None),
    };

    if !info.is_initialized {
        return Ok(None);
    }

    let mint_authority_present = info.mint_authority.is_some();
    let freeze_authority_present = info.freeze_authority.is_some();

    let holders = rpc
        .token_largest_accounts(mint)
        .await
        .map_err(|e| anyhow::anyhow!("token_largest_accounts RPC failed: {e}"))?;

    let holder_top10_pct = if info.supply > 0 && !holders.is_empty() {
        let top10_sum: u128 = holders.iter().take(10).map(|h| h.amount as u128).sum();
        let pct_x100 = (top10_sum * 10000 / info.supply as u128) as u32;
        Decimal::from_parts(pct_x100, 0, 0, false, 2)
    } else {
        return Ok(None);
    };

    // Token age: walk the mint account's signature history backward until the
    // age is proven (a signature older than the minimum age is found) or the
    // history is exhausted (the oldest signature is the true creation
    // activity). If the page bound is reached without proof, the age cannot
    // be verified and the candidate is rejected — never guessed.
    let now_ts = Utc::now().timestamp();
    let cutoff = now_ts.saturating_sub(min_token_age_secs.max(0));
    let mut oldest: Option<i64> = None;
    let mut before: Option<String> = None;
    let mut exhausted = false;
    for _ in 0..MAX_AGE_PAGES {
        let page = rpc
            .signatures_for_address_paged(mint, AGE_PAGE_LIMIT, before.as_deref())
            .await
            .map_err(|e| anyhow::anyhow!("signatures_for_address RPC failed: {e}"))?;
        if page.is_empty() {
            exhausted = true;
            break;
        }
        for entry in &page {
            if let Some(bt) = entry.block_time {
                oldest = Some(match oldest {
                    Some(prev) => prev.min(bt),
                    None => bt,
                });
            }
        }
        if oldest.is_some_and(|o| o <= cutoff) {
            break;
        }
        let last_sig = page.last().map(|s| s.signature.clone());
        if (page.len() as u32) < AGE_PAGE_LIMIT {
            exhausted = true;
            break;
        }
        match last_sig {
            Some(s) => before = Some(s),
            None => {
                exhausted = true;
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    }

    let Some(oldest_ts) = oldest else {
        return Ok(None);
    };
    if !exhausted && oldest_ts > cutoff {
        tracing::info!(
            mint = %mint,
            "token age could not be verified within the page bound; candidate rejected"
        );
        return Ok(None);
    }
    let token_age_secs = now_ts.saturating_sub(oldest_ts);

    // sellable and route_available are confirmed by the fact that we successfully
    // fetched a Jupiter quote for this mint during candidate generation.
    // All other fields are unknown from the chain alone; we do not mark them as
    // safe when we cannot verify them.
    let now = Utc::now();
    Ok(Some(TokenSafety {
        mint_authority_present,
        freeze_authority_present,
        holder_top10_pct,
        token_age_secs,
        liquidity_locked_or_burned: None,
        sellable: None,
        route_available: None,
        creator_suspicious: None,
        abnormal_activity: None,
        liquidity_change_pct: None,
        observed_at: now,
    }))
}

/// Fetch real market snapshot using a Jupiter quote for pricing + RPC data.
/// Liquidity is estimated from Jupiter's price impact: if a trade of size X
/// causes Y% price impact, the effective pool liquidity is approximately X/Y.
#[allow(clippy::too_many_arguments)]
pub async fn fetch_market_snapshot(
    executor: &dyn crate::execution::Executor,
    mint: &str,
    sol_price_usd: Decimal,
    sol_decimals: u8,
    token_decimals: u8,
    input_amount: u64,
    slippage_bps: u16,
) -> Result<Option<(MarketSnapshot, u32)>, anyhow::Error> {
    let quote = executor
        .quote(WSOL_MINT, mint, input_amount, slippage_bps)
        .await
        .map_err(|e| anyhow::anyhow!("Jupiter quote failed: {e}"))?;

    if quote.output_amount == 0 || quote.input_amount == 0 {
        return Ok(None);
    }

    let sol_spent =
        Decimal::from(quote.input_amount) / Decimal::from(10u64.pow(sol_decimals as u32));
    let tokens_received =
        Decimal::from(quote.output_amount) / Decimal::from(10u64.pow(token_decimals as u32));

    if tokens_received.is_zero() {
        return Ok(None);
    }

    let price_usd = sol_spent * sol_price_usd / tokens_received;

    // Real liquidity estimation from Jupiter's price impact.
    let price_impact_bps = quote.price_impact_bps;
    let trade_size_usd = sol_spent * sol_price_usd;
    let liquidity_usd = estimate_liquidity_usd(trade_size_usd, price_impact_bps);

    let now = Utc::now();
    Ok(Some((
        MarketSnapshot {
            mint: mint.to_string(),
            price_usd,
            liquidity_usd,
            volume_24h_usd: Decimal::ZERO,
            volatility_pct: Decimal::ZERO,
            buy_sell_imbalance: Decimal::ZERO,
            observed_at: now,
            received_at: now,
            slot: None,
            price_impact_bps: Some(price_impact_bps),
        },
        price_impact_bps,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn zero_impact_returns_zero_liquidity() {
        // Zero price impact means the trade is negligible relative to pool
        // depth. We cannot estimate liquidity, so we return zero (fail closed).
        let liq = estimate_liquidity_usd(dec!(100), 0);
        assert_eq!(liq, Decimal::ZERO);
    }

    #[test]
    fn positive_impact_estimates_liquidity() {
        // $100 trade causing 100 bps (1%) impact implies ~$10,000 liquidity.
        let liq = estimate_liquidity_usd(dec!(100), 100);
        assert_eq!(liq, dec!(10000));
    }

    #[test]
    fn small_impact_implies_large_liquidity() {
        // $100 trade causing 1 bps impact implies ~$1,000,000 liquidity.
        let liq = estimate_liquidity_usd(dec!(100), 1);
        assert_eq!(liq, dec!(1000000));
    }

    #[test]
    fn large_impact_implies_small_liquidity() {
        // $100 trade causing 1000 bps (10%) impact implies ~$1,000 liquidity.
        let liq = estimate_liquidity_usd(dec!(100), 1000);
        assert_eq!(liq, dec!(1000));
    }
}
