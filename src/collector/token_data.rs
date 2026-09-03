use crate::data::rpc::RpcPool;
use crate::domain::market::MarketSnapshot;
use crate::domain::token::TokenSafety;
use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

/// Fetch real token safety data from the Solana chain via RPC.
///
/// Returns `Err` if any required RPC call fails. Returns `Ok(None)` if the
/// mint account does not exist.
pub async fn fetch_token_safety(
    rpc: &RpcPool,
    mint: &str,
    _min_token_age_secs: i64,
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

    let sigs = rpc
        .signatures_for_address(mint, 1)
        .await
        .map_err(|e| anyhow::anyhow!("signatures_for_address RPC failed: {e}"))?;

    let now_ts = Utc::now().timestamp();
    let token_age_secs = if let Some(oldest) = sigs.last() {
        if let Some(bt) = oldest.block_time {
            now_ts.saturating_sub(bt)
        } else {
            return Ok(None);
        }
    } else {
        return Ok(None);
    };

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
pub async fn fetch_market_snapshot(
    rpc: &RpcPool,
    executor: &dyn crate::execution::Executor,
    mint: &str,
    sol_price_usd: Decimal,
    sol_decimals: u8,
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
    let tokens_received = Decimal::from(quote.output_amount)
        / Decimal::from(10u64.pow(quote_mint_decimals(rpc, mint).await.unwrap_or(6) as u32));

    if tokens_received.is_zero() {
        return Ok(None);
    }

    let price_usd = sol_spent * sol_price_usd / tokens_received;

    // Real liquidity estimation from Jupiter's price impact.
    // price_impact_bps = (trade_size_usd / pool_liquidity_usd) * 10000
    // => pool_liquidity_usd = (trade_size_usd * 10000) / price_impact_bps
    let price_impact_bps = quote.price_impact_bps;
    let trade_size_usd = sol_spent * sol_price_usd;
    let liquidity_usd = if price_impact_bps > 0 {
        (trade_size_usd * dec!(10000) / Decimal::from(price_impact_bps)).round_dp(2)
    } else {
        // Zero price impact means very deep liquidity; use a large but bounded estimate.
        dec!(10_000_000)
    };

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

async fn quote_mint_decimals(rpc: &RpcPool, mint: &str) -> Option<u8> {
    rpc.mint_account_info(mint)
        .await
        .ok()
        .flatten()
        .map(|i| i.decimals)
}
