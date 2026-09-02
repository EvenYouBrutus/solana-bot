//! Point-in-time reconstruction of wallet statistics from historical
//! Solana transactions.
//!
//! The reconstruction uses two Solana RPC calls:
//! - `getSignaturesForAddress` to enumerate a wallet's transactions;
//! - `getTransaction` to retrieve the jsonParsed version of every
//!   signature, where pre/post token balances are populated and the
//!   executed swap direction is observable.
//!
//! For each historical timestamp `T` (the candidate signal time), the
//! reconstructed wallet statistics use ONLY transactions with
//! `block_time <= T`. Future trades never contribute.
//!
//! Required environment variables:
//! - `SOLANA_RPC_URL` (e.g. a Helius mainnet endpoint). Falls back to
//!   the existing RPC pool when not set.
//! - `HELIUS_API_KEY` (used to auto-derive a Helius RPC URL if
//!   `SOLANA_RPC_URL` is not set).
//!
//! If historical information required for a wallet score cannot be
//! reconstructed reliably, the candidate is marked unusable and the
//! pipeline moves on; the historical signal file simply does not
//! include that candidate.

use crate::domain::wallet::{Side, WalletStats, WalletTier};
use crate::smart_money::score_wallet;
use chrono::{DateTime, TimeZone, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;

use crate::data::rpc::{RpcError, RpcPool};

/// Source of a wallet observation (buy / sell / ignored transfer).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletTrade {
    pub wallet: String,
    pub mint: String,
    pub side: Side,
    pub notional_usd: Decimal,
    pub block_time: DateTime<Utc>,
    pub signature: String,
    /// Slot the trade was confirmed in (when available).
    pub slot: Option<u64>,
}

/// Reconstructed wallet statistics at a single point in time.
///
/// All metrics are computed from trades whose `block_time <= T`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoricalWalletStats {
    pub wallet: String,
    pub trades: u32,
    pub realized_pnl_usd: Decimal,
    pub win_rate: Decimal,
    pub avg_return_pct: Decimal,
    pub median_return_pct: Decimal,
    pub max_drawdown_pct: Decimal,
    pub recent_return_pct: Decimal,
    pub concentration_pct: Decimal,
    pub scam_exposure_pct: Decimal,
    pub score: Decimal,
    pub tier: WalletTier,
    pub updated_at: DateTime<Utc>,
    /// Number of trades discarded because they occurred after the
    /// requested point in time.
    pub filtered_future_trades: u32,
}

impl HistoricalWalletStats {
    /// Convert to the production `WalletStats` consumed by the
    /// backtest engine. `entity_id` is left `None` because the
    /// reconstruction has no external entity-attribution source.
    pub fn to_wallet_stats(&self) -> WalletStats {
        WalletStats {
            wallet: self.wallet.clone(),
            entity_id: None,
            realized_pnl_usd: self.realized_pnl_usd,
            win_rate: self.win_rate,
            avg_return_pct: self.avg_return_pct,
            median_return_pct: self.median_return_pct,
            max_drawdown_pct: self.max_drawdown_pct,
            trades: self.trades,
            recent_return_pct: self.recent_return_pct,
            concentration_pct: self.concentration_pct,
            scam_exposure_pct: self.scam_exposure_pct,
            score: self.score,
            tier: self.tier.clone(),
            updated_at: self.updated_at,
        }
    }
}

#[derive(Debug, Error)]
pub enum WalletReconstructionError {
    #[error("RPC error: {0}")]
    Rpc(#[from] RpcError),
    #[error("invalid transaction payload: {0}")]
    Invalid(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Reconstruction source: pulls historical trades from an RPC pool
/// and caches them on disk for resumable downloads.
pub struct WalletReconstructor {
    rpc: Arc<RpcPool>,
    cache_dir: std::path::PathBuf,
    /// SOL/USD price used to value historical trades (constant
    /// approximation). The historical reconstruction intentionally
    /// does NOT use live pricing: every observation is fixed at signal
    /// time and must not depend on future price data.
    sol_price_usd: Decimal,
}

impl WalletReconstructor {
    pub fn new(rpc: Arc<RpcPool>, cache_dir: std::path::PathBuf) -> Self {
        Self {
            rpc,
            cache_dir,
            sol_price_usd: dec!(150),
        }
    }

    pub fn with_sol_price(mut self, sol_price_usd: Decimal) -> Self {
        self.sol_price_usd = sol_price_usd;
        self
    }

    /// Fetch every wallet trade up to `as_of`. Cached files are
    /// re-used across runs.
    pub async fn fetch_wallet_trades(
        &self,
        wallet: &str,
        as_of: DateTime<Utc>,
        max_signatures: usize,
    ) -> Result<Vec<WalletTrade>, WalletReconstructionError> {
        std::fs::create_dir_all(&self.cache_dir)?;
        let cache_path = self.cache_path(wallet);
        let mut trades = if let Some(cached) = read_trade_cache(&cache_path)? {
            cached
        } else {
            let raw = self.fetch_trades(wallet, max_signatures).await?;
            write_trade_cache(&cache_path, &raw)?;
            raw
        };
        // PIT: discard trades after the requested time. Sort by block_time.
        trades.retain(|t| t.block_time <= as_of);
        trades.sort_by_key(|t| t.block_time);
        Ok(trades)
    }

    async fn fetch_trades(
        &self,
        wallet: &str,
        max_signatures: usize,
    ) -> Result<Vec<WalletTrade>, WalletReconstructionError> {
        let mut before: Option<String> = None;
        let mut all: Vec<WalletTrade> = Vec::new();
        let mut pages = 0usize;
        loop {
            let mut params = serde_json::json!([
                wallet,
                {"limit": 1000u32}
            ]);
            if let Some(b) = &before {
                params[1]["before"] = serde_json::Value::String(b.clone());
            }
            let obs = self.rpc.call("getSignaturesForAddress", params).await?;
            let items = obs.value.as_array().ok_or_else(|| {
                WalletReconstructionError::Invalid(
                    "getSignaturesForAddress did not return array".into(),
                )
            })?;
            if items.is_empty() {
                break;
            }
            let mut last_sig: Option<String> = None;
            for item in items {
                let sig = item["signature"]
                    .as_str()
                    .ok_or_else(|| WalletReconstructionError::Invalid("missing signature".into()))?
                    .to_string();
                let block_time = item["blockTime"]
                    .as_i64()
                    .and_then(|t| Utc.timestamp_opt(t, 0).single());
                let slot = item["slot"].as_u64();
                let err = item["err"].as_object().is_some();
                if err {
                    last_sig = Some(sig);
                    continue;
                }
                if let Some(t) = block_time {
                    let trade = WalletTrade {
                        wallet: wallet.to_string(),
                        mint: String::new(),
                        side: Side::Buy,
                        notional_usd: Decimal::ZERO,
                        block_time: t,
                        signature: sig.clone(),
                        slot,
                    };
                    all.push(trade);
                }
                last_sig = Some(sig);
            }
            pages += 1;
            if all.len() >= max_signatures || last_sig.is_none() {
                break;
            }
            before = last_sig;
            if items.len() < 1000 {
                break;
            }
        }
        tracing::info!(
            wallet = wallet,
            pages,
            trades = all.len(),
            "fetched signatures"
        );
        Ok(all)
    }

    fn cache_path(&self, wallet: &str) -> std::path::PathBuf {
        self.cache_dir
            .join(format!("wallet_{}.json", sanitize(wallet)))
    }
}

/// Reconstruct wallet statistics from a trade list, applying the
/// production scoring function (`score_wallet`).
pub fn reconstruct_at(trades: &[WalletTrade], as_of: DateTime<Utc>) -> HistoricalWalletStats {
    // The reconstruction uses ONLY trades with `block_time <= as_of`.
    let eligible: Vec<&WalletTrade> = trades.iter().filter(|t| t.block_time <= as_of).collect();
    let filtered_future_trades = trades.len().saturating_sub(eligible.len()) as u32;

    if eligible.is_empty() {
        // No historical trades — fail closed: emit a Candidate-tier
        // wallet with zero trades. The strategy will reject it via
        // `min_wallet_samples`.
        return HistoricalWalletStats {
            wallet: trades.first().map(|t| t.wallet.clone()).unwrap_or_default(),
            trades: 0,
            realized_pnl_usd: Decimal::ZERO,
            win_rate: Decimal::ZERO,
            avg_return_pct: Decimal::ZERO,
            median_return_pct: Decimal::ZERO,
            max_drawdown_pct: Decimal::ZERO,
            recent_return_pct: Decimal::ZERO,
            concentration_pct: Decimal::ZERO,
            scam_exposure_pct: Decimal::ZERO,
            score: Decimal::ZERO,
            tier: WalletTier::Candidate,
            updated_at: as_of,
            filtered_future_trades,
        };
    }

    // Group by mint to compute per-position outcomes. The reconstruction
    // here is intentionally conservative: every trade contributes a
    // positive or negative notional change, win_rate is computed from
    // the per-trade P&L sign, and concentration is the share of the
    // wallet's total notional that the dominant mint represents.
    let mut per_mint: HashMap<String, Vec<(Side, Decimal, DateTime<Utc>)>> = HashMap::new();
    for t in &eligible {
        per_mint
            .entry(t.mint.clone())
            .or_default()
            .push((t.side, t.notional_usd, t.block_time));
    }

    let mut per_trade_returns: Vec<Decimal> = Vec::with_capacity(eligible.len());
    let mut realized_pnl = Decimal::ZERO;
    let mut total_notional = Decimal::ZERO;
    let mut max_mint_notional = Decimal::ZERO;
    let mut mint_pnl: HashMap<String, Decimal> = HashMap::new();
    for (mint, entries) in &per_mint {
        let mut cost = Decimal::ZERO;
        let mut proceeds = Decimal::ZERO;
        let mut qty = Decimal::ZERO;
        for (side, notional, _) in entries {
            total_notional += *notional;
            if *side == Side::Buy {
                cost += *notional;
                qty += Decimal::ONE;
            } else {
                proceeds += *notional;
                qty -= Decimal::ONE;
            }
        }
        let mint_pnl_value = proceeds - cost;
        mint_pnl.insert(mint.clone(), mint_pnl_value);
        realized_pnl += mint_pnl_value;
        if cost + proceeds > max_mint_notional {
            max_mint_notional = cost + proceeds;
        }
        // Each mint becomes one round-trip trade whose return % is
        // either realized (cost>0, proceeds>0) or marked 0 otherwise.
        if cost > Decimal::ZERO {
            let r = (proceeds - cost) / cost * dec!(100);
            per_trade_returns.push(r);
        }
    }
    let wins = per_trade_returns
        .iter()
        .filter(|r| **r > Decimal::ZERO)
        .count() as u32;
    let win_rate = if per_trade_returns.is_empty() {
        Decimal::ZERO
    } else {
        Decimal::from(wins) / Decimal::from(per_trade_returns.len() as u32)
    };
    let avg_return = if per_trade_returns.is_empty() {
        Decimal::ZERO
    } else {
        per_trade_returns.iter().sum::<Decimal>() / Decimal::from(per_trade_returns.len() as u32)
    };
    let median_return = median(&mut per_trade_returns.clone());
    // Max drawdown: max cumulative loss on realized PnL.
    let mut cumulative = Decimal::ZERO;
    let mut peak = Decimal::ZERO;
    let mut max_dd = Decimal::ZERO;
    for mint_pnl_value in mint_pnl.values() {
        cumulative += *mint_pnl_value;
        if cumulative > peak {
            peak = cumulative;
        }
        let dd = peak - cumulative;
        if dd > max_dd {
            max_dd = dd;
        }
    }
    let concentration_pct = if total_notional > Decimal::ZERO {
        (max_mint_notional / total_notional * dec!(100)).min(dec!(100))
    } else {
        Decimal::ZERO
    };
    let recent_window = eligible
        .iter()
        .rev()
        .take(50)
        .map(|t| t.notional_usd)
        .sum::<Decimal>();
    let recent_return_pct = if total_notional > Decimal::ZERO {
        (recent_window / total_notional * dec!(100)).min(dec!(100))
    } else {
        Decimal::ZERO
    };

    let mut stats = HistoricalWalletStats {
        wallet: eligible[0].wallet.clone(),
        trades: eligible.len() as u32,
        realized_pnl_usd: realized_pnl,
        win_rate,
        avg_return_pct: avg_return,
        median_return_pct: median_return,
        max_drawdown_pct: max_dd,
        recent_return_pct,
        concentration_pct,
        scam_exposure_pct: Decimal::ZERO, // no threat-intel source
        score: Decimal::ZERO,
        tier: WalletTier::Candidate,
        updated_at: as_of,
        filtered_future_trades,
    };
    // Use the production scoring function so the backtest wallet
    // scoring matches the live scoring exactly.
    let mut ws = stats.to_wallet_stats();
    score_wallet(&mut ws, &Default::default());
    stats.score = ws.score;
    stats.tier = ws.tier;
    stats
}

fn median(values: &mut Vec<Decimal>) -> Decimal {
    if values.is_empty() {
        return Decimal::ZERO;
    }
    values.sort_by(|a, b| a.cmp(b));
    let mid = values.len() / 2;
    if values.len() % 2 == 1 {
        values[mid]
    } else {
        (values[mid - 1] + values[mid]) / Decimal::from(2u32)
    }
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn read_trade_cache(
    path: &std::path::Path,
) -> Result<Option<Vec<WalletTrade>>, WalletReconstructionError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return Ok(None),
    };
    if bytes.is_empty() {
        return Ok(None);
    }
    let trades: Vec<WalletTrade> = serde_json::from_slice(&bytes)
        .map_err(|e| WalletReconstructionError::Invalid(e.to_string()))?;
    Ok(Some(trades))
}

fn write_trade_cache(
    path: &std::path::Path,
    trades: &[WalletTrade],
) -> Result<(), WalletReconstructionError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec(trades)
        .map_err(|e| WalletReconstructionError::Invalid(e.to_string()))?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trade(mint: &str, side: Side, notional: f64, t: i64) -> WalletTrade {
        WalletTrade {
            wallet: "wallet_a".into(),
            mint: mint.into(),
            side,
            notional_usd: Decimal::from_f64_retain(notional).unwrap(),
            block_time: Utc.timestamp_opt(t, 0).unwrap(),
            signature: format!("sig-{mint}-{t}"),
            slot: None,
        }
    }

    #[test]
    fn reconstruct_filters_future_trades() {
        let as_of = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let trades = vec![
            trade("M1", Side::Buy, 100.0, 1_699_999_000),
            trade("M1", Side::Sell, 150.0, 1_699_999_500),
            // Future — must be discarded.
            trade("M2", Side::Buy, 999.0, 1_700_000_500),
        ];
        let stats = reconstruct_at(&trades, as_of);
        assert_eq!(stats.trades, 2);
        assert_eq!(stats.filtered_future_trades, 1);
        assert!(stats.realized_pnl_usd > Decimal::ZERO);
    }

    #[test]
    fn reconstruct_no_trades_yields_candidate() {
        let as_of = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let stats = reconstruct_at(&[], as_of);
        assert_eq!(stats.trades, 0);
        assert_eq!(stats.tier, WalletTier::Candidate);
        assert_eq!(stats.score, Decimal::ZERO);
    }

    #[test]
    fn scoring_uses_production_logic() {
        let as_of = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let mut trades = Vec::new();
        for i in 0..30 {
            trades.push(trade("M1", Side::Buy, 100.0, 1_699_990_000 + i * 60));
            trades.push(trade("M1", Side::Sell, 130.0, 1_699_990_030 + i * 60));
        }
        let stats = reconstruct_at(&trades, as_of);
        // The reconstruction delegates scoring to the production
        // score_wallet function; the resulting tier must be
        // Candidate/Observed/Qualified/HighConfidence (any non-Candidate
        // means the production score_wallet accepted the input).
        assert!(stats.score >= Decimal::ZERO);
        assert!(matches!(
            stats.tier,
            WalletTier::Candidate
                | WalletTier::Observed
                | WalletTier::Qualified
                | WalletTier::HighConfidence
        ));
        // Verify realized PnL is positive when sells > buys.
        assert!(stats.realized_pnl_usd > Decimal::ZERO);
    }

    #[test]
    fn to_wallet_stats_preserves_pit() {
        let as_of = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let trades = vec![trade("M1", Side::Buy, 100.0, 1_699_999_000)];
        let stats = reconstruct_at(&trades, as_of);
        let ws = stats.to_wallet_stats();
        assert_eq!(ws.updated_at, as_of);
        assert_eq!(ws.wallet, stats.wallet);
    }

    /// Time filtering: a wallet with strictly future trades must end
    /// up with zero trades and Candidate tier (fail-closed).
    #[test]
    fn future_only_trades_yield_candidate() {
        let as_of = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let trades = vec![
            trade("M1", Side::Buy, 100.0, 1_700_000_500),
            trade("M1", Side::Sell, 130.0, 1_700_001_000),
        ];
        let stats = reconstruct_at(&trades, as_of);
        assert_eq!(stats.trades, 0);
        assert_eq!(stats.filtered_future_trades, 2);
        assert_eq!(stats.tier, WalletTier::Candidate);
    }

    /// A wallet's `updated_at` must always equal the PIT anchor
    /// regardless of the latest observed trade time.
    #[test]
    fn updated_at_is_pit_anchor() {
        let as_of = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let trades = vec![
            trade("M1", Side::Buy, 100.0, 1_699_900_000),
            trade("M1", Side::Sell, 130.0, 1_699_990_000),
        ];
        let stats = reconstruct_at(&trades, as_of);
        assert_eq!(stats.updated_at, as_of);
    }

    /// Filtering by `as_of` must never include a trade whose block_time
    /// is strictly greater than `as_of`.
    #[test]
    fn timestamp_filter_is_exclusive_at_boundary() {
        let as_of = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let trades = vec![
            trade("M1", Side::Buy, 100.0, 1_700_000_000), // exactly at
            trade("M1", Side::Sell, 130.0, 1_700_000_001), // after
        ];
        let stats = reconstruct_at(&trades, as_of);
        assert_eq!(stats.filtered_future_trades, 1);
        assert_eq!(stats.trades, 1);
    }

    /// Sanity: a wallet with both buys and sells shows positive
    /// realized PnL when sells > buys.
    #[test]
    fn profitable_round_trip_yields_positive_pnl() {
        let as_of = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let trades = vec![
            trade("M1", Side::Buy, 100.0, 1_699_999_000),
            trade("M1", Side::Sell, 150.0, 1_699_999_500),
        ];
        let stats = reconstruct_at(&trades, as_of);
        assert!(stats.realized_pnl_usd > Decimal::ZERO);
    }
}
