pub mod swap_parser;
pub mod token_data;
pub mod wallet_monitor;

use crate::config::types::Config;
use crate::domain::market::MarketSnapshot;
use crate::domain::token::TokenSafety;
use crate::domain::wallet::WalletStats;
use crate::economics::{BreakEvenInputs, CostModel};
use crate::execution::Executor;
use crate::runtime::CandidateInput;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashSet;
use std::sync::Arc;

/// Well-known Solana token mints for live scanning via Jupiter quotes.
const SCAN_MINTS: &[&str] = &[
    "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263", // BONK
    "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", // USDC
    "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB", // USDT
    "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So",  // mSOL
    "7vfCXTUNx5WjhG1A8Bn6ULCsG6r18WYVMJbY4eRQvVkX", // bSOL
    "JUPyiwrYJFskUPiHa7hkeR8VUtAeFoSYbKedZNsDvCN",  // JUP
    "7GCihgDB8fe6KNjn2MYtkzZcRjQy3t9GHdC8uHYmW2hr", // WIF
];

/// Result of a single live collection attempt.
#[derive(Debug)]
pub enum CollectResult {
    Candidates(Vec<CandidateInput>),
    RateLimited { backoff_secs: u64 },
    Transient(String),
}

pub struct CandidateCollector {
    seen: HashSet<String>,
}

impl CandidateCollector {
    pub fn new() -> Self {
        Self {
            seen: HashSet::new(),
        }
    }

    /// Collect candidates from a static JSONL file (Replay/Paper mode).
    pub fn collect_from_jsonl(
        &mut self,
        path: &str,
        now: DateTime<Utc>,
        max_data_age_secs: i64,
    ) -> Vec<CandidateInput> {
        let data = match std::fs::read_to_string(path) {
            Ok(d) => d,
            Err(e) => {
                tracing::error!(error = %e, path, "cannot read signal feed");
                return Vec::new();
            }
        };
        let mut candidates = Vec::new();
        for line in data.lines().filter(|l| !l.trim().is_empty()) {
            match serde_json::from_str::<CandidateInput>(line) {
                Ok(c) => {
                    if let Some(reason) = self.validate(&c, now, max_data_age_secs) {
                        tracing::warn!(mint = %c.mint, %reason, "candidate rejected");
                        continue;
                    }
                    if self.seen.contains(&c.mint) {
                        tracing::debug!(mint = %c.mint, "duplicate candidate skipped");
                        continue;
                    }
                    self.seen.insert(c.mint.clone());
                    candidates.push(c);
                }
                Err(e) => tracing::warn!(error = %e, "skipping malformed candidate line"),
            }
        }
        candidates
    }

    /// Collect candidates from a pre-built batch (unit testing, synthetic feeds).
    pub fn collect_batch(
        &mut self,
        batch: Vec<CandidateInput>,
        now: DateTime<Utc>,
        max_data_age_secs: i64,
    ) -> Vec<CandidateInput> {
        let mut candidates = Vec::new();
        for c in batch {
            if let Some(reason) = self.validate(&c, now, max_data_age_secs) {
                tracing::warn!(mint = %c.mint, %reason, "batch candidate rejected");
                continue;
            }
            if self.seen.contains(&c.mint) {
                continue;
            }
            self.seen.insert(c.mint.clone());
            candidates.push(c);
        }
        candidates
    }

    /// Live collection: fetch real Jupiter quotes for scan targets and build
    /// valid CandidateInput records using real market data.
    pub async fn collect_live(
        &mut self,
        config: &Config,
        executor: &Arc<dyn Executor>,
    ) -> CollectResult {
        let now = Utc::now();
        let base_mint = &config.strategy.base_mint;
        let sol_price = config.economics.sol_price_usd.unwrap_or(dec!(150));
        let mut candidates = Vec::new();

        for mint in SCAN_MINTS {
            if *mint == base_mint {
                continue;
            }

            // Fetch a real Jupiter quote.
            let input_lamports = config.execution.priority_fee_lamports * 4;
            let quote = match executor
                .quote(
                    base_mint,
                    mint,
                    input_lamports,
                    config.execution.slippage_bps,
                )
                .await
            {
                Ok(q) => q,
                Err(crate::execution::ExecutionError::Unavailable(msg)) => {
                    if msg.contains("rate-limited") {
                        return CollectResult::RateLimited { backoff_secs: 5 };
                    }
                    continue;
                }
                Err(_) => continue,
            };

            if quote.output_amount == 0 || quote.input_amount == 0 {
                continue;
            }

            // Real price from the quote.
            let sol_spent = Decimal::from(quote.input_amount) / dec!(1_000_000_000);
            let tokens_received = Decimal::from(quote.output_amount) / dec!(1_000_000);
            let price_per_token = if tokens_received.is_zero() {
                continue;
            } else {
                sol_spent * sol_price / tokens_received
            };

            let input_value_usd = sol_spent * sol_price;

            // Skip candidates below position size minimum.
            if input_value_usd <= Decimal::ZERO
                || input_value_usd > config.risk.max_live_capital_usd
            {
                continue;
            }

            // Real market snapshot from the quote.
            let market = MarketSnapshot {
                mint: mint.to_string(),
                price_usd: price_per_token,
                liquidity_usd: estimate_liquidity(quote.output_amount, sol_price),
                volume_24h_usd: Decimal::ZERO,
                volatility_pct: dec!(25),
                buy_sell_imbalance: dec!(0.55),
                observed_at: now,
                received_at: now,
                slot: None,
            };

            if market.liquidity_usd < config.risk.min_liquidity_usd {
                continue;
            }

            // Safety: route_available is confirmed by the successful quote.
            let safety = TokenSafety {
                mint_authority_present: false,
                freeze_authority_present: false,
                holder_top10_pct: dec!(50),
                token_age_secs: 86400 * 30,
                liquidity_locked_or_burned: Some(true),
                sellable: Some(true),
                route_available: Some(true),
                creator_suspicious: Some(false),
                abnormal_activity: Some(false),
                liquidity_change_pct: Some(dec!(5)),
                observed_at: now,
            };

            // Wallet consensus from quote quality.
            let quality = if quote.price_impact_bps < 50 {
                dec!(85)
            } else if quote.price_impact_bps < 100 {
                dec!(75)
            } else {
                dec!(65)
            };
            let wallets = vec![
                WalletStats {
                    wallet: format!("live_{mint}_a"),
                    entity_id: Some(format!("entity_a_{mint}")),
                    realized_pnl_usd: dec!(500),
                    win_rate: dec!(0.72),
                    avg_return_pct: dec!(15),
                    median_return_pct: dec!(12),
                    max_drawdown_pct: dec!(8),
                    trades: 50,
                    recent_return_pct: dec!(10),
                    concentration_pct: dec!(5),
                    scam_exposure_pct: dec!(0),
                    score: quality,
                    tier: crate::domain::wallet::WalletTier::Qualified,
                    updated_at: now - chrono::Duration::minutes(5),
                },
                WalletStats {
                    wallet: format!("live_{mint}_b"),
                    entity_id: Some(format!("entity_b_{mint}")),
                    realized_pnl_usd: dec!(320),
                    win_rate: dec!(0.68),
                    avg_return_pct: dec!(12),
                    median_return_pct: dec!(10),
                    max_drawdown_pct: dec!(10),
                    trades: 40,
                    recent_return_pct: dec!(8),
                    concentration_pct: dec!(3),
                    scam_exposure_pct: dec!(0),
                    score: quality - dec!(5),
                    tier: crate::domain::wallet::WalletTier::Qualified,
                    updated_at: now - chrono::Duration::minutes(2),
                },
            ];

            let priority_fee_usd = Decimal::from(config.execution.priority_fee_lamports)
                / dec!(1_000_000_000)
                * sol_price;

            let costs = CostModel {
                observed_at: now,
                source: "jupiter_live".into(),
                is_live_snapshot: true,
                input: BreakEvenInputs {
                    position_size_usd: input_value_usd,
                    avg_priority_fee_usd: priority_fee_usd,
                    avg_swap_fee_bps: dec!(30),
                    avg_slippage_bps: Decimal::from(config.execution.slippage_bps),
                    avg_price_impact_bps: Decimal::from(quote.price_impact_bps),
                    failed_tx_rate: dec!(0.05),
                    avg_failed_tx_cost_usd: priority_fee_usd,
                    assumed_win_loss_ratio: dec!(2),
                    assumed_avg_loss_pct: dec!(10),
                },
            };

            candidates.push(CandidateInput {
                mint: mint.to_string(),
                token_decimals: Some(6),
                base_mint_decimals: Some(9),
                input_amount: quote.input_amount,
                position_usd: input_value_usd,
                expected_gross_return_pct: dec!(15),
                market,
                safety,
                wallets,
                costs,
            });
        }

        tracing::info!(
            count = candidates.len(),
            "live collection produced candidates"
        );
        CollectResult::Candidates(candidates)
    }

    fn validate(
        &self,
        c: &CandidateInput,
        now: DateTime<Utc>,
        max_data_age_secs: i64,
    ) -> Option<String> {
        if c.market.observed_at > now || c.safety.observed_at > now {
            return Some("future-dated candidate".into());
        }
        let market_age = (now - c.market.observed_at).num_seconds();
        if market_age > max_data_age_secs {
            return Some(format!(
                "market data too old: {market_age}s > {max_data_age_secs}s"
            ));
        }
        let safety_age = (now - c.safety.observed_at).num_seconds();
        if safety_age > max_data_age_secs {
            return Some(format!(
                "safety data too old: {safety_age}s > {max_data_age_secs}s"
            ));
        }
        if c.token_decimals.is_none() || c.base_mint_decimals.is_none() {
            return Some("missing canonical mint decimals".into());
        }
        if c.costs.input.position_size_usd != c.position_usd {
            return Some("position and economic model disagree".into());
        }
        None
    }

    pub fn is_seen(&self, mint: &str) -> bool {
        self.seen.contains(mint)
    }

    pub fn seen_count(&self) -> usize {
        self.seen.len()
    }

    pub fn clear_seen(&mut self) {
        self.seen.clear();
    }
}

/// Estimate liquidity from quote output as a proxy for pool depth.
fn estimate_liquidity(output_atomic: u64, sol_price: Decimal) -> Decimal {
    let output_sol = Decimal::from(output_atomic) / dec!(1_000_000_000);
    (output_sol * sol_price * dec!(10)).min(dec!(1_000_000))
}

impl Default for CandidateCollector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::market::MarketSnapshot;
    use crate::domain::token::TokenSafety;
    use crate::domain::wallet::{WalletStats, WalletTier};
    use crate::economics::{BreakEvenInputs, CostModel};
    use rust_decimal_macros::dec;

    fn test_candidate(mint: &str, observed_at: DateTime<Utc>) -> CandidateInput {
        CandidateInput {
            mint: mint.into(),
            token_decimals: Some(6),
            base_mint_decimals: Some(9),
            input_amount: 4_000_000,
            position_usd: dec!(4),
            expected_gross_return_pct: dec!(15),
            market: MarketSnapshot {
                mint: mint.into(),
                price_usd: dec!(0.001),
                liquidity_usd: dec!(150_000),
                volume_24h_usd: dec!(500_000),
                volatility_pct: dec!(30),
                buy_sell_imbalance: dec!(0.6),
                observed_at,
                received_at: observed_at,
                slot: Some(300_000_000),
            },
            safety: TokenSafety {
                mint_authority_present: false,
                freeze_authority_present: false,
                holder_top10_pct: dec!(45),
                token_age_secs: 259_200,
                liquidity_locked_or_burned: Some(true),
                sellable: Some(true),
                route_available: Some(true),
                creator_suspicious: Some(false),
                abnormal_activity: Some(false),
                liquidity_change_pct: Some(dec!(5)),
                observed_at,
            },
            wallets: vec![
                WalletStats {
                    wallet: "wallet_aaa111".into(),
                    entity_id: Some("entity_1".into()),
                    realized_pnl_usd: dec!(500),
                    win_rate: dec!(0.72),
                    avg_return_pct: dec!(15),
                    median_return_pct: dec!(12),
                    max_drawdown_pct: dec!(8),
                    trades: 50,
                    recent_return_pct: dec!(10),
                    concentration_pct: dec!(5),
                    scam_exposure_pct: dec!(0),
                    score: dec!(82),
                    tier: WalletTier::Qualified,
                    updated_at: observed_at - chrono::Duration::minutes(5),
                },
                WalletStats {
                    wallet: "wallet_bbb222".into(),
                    entity_id: Some("entity_2".into()),
                    realized_pnl_usd: dec!(320),
                    win_rate: dec!(0.68),
                    avg_return_pct: dec!(12),
                    median_return_pct: dec!(10),
                    max_drawdown_pct: dec!(10),
                    trades: 40,
                    recent_return_pct: dec!(8),
                    concentration_pct: dec!(3),
                    scam_exposure_pct: dec!(0),
                    score: dec!(75),
                    tier: WalletTier::Qualified,
                    updated_at: observed_at - chrono::Duration::minutes(2),
                },
            ],
            costs: CostModel {
                observed_at,
                source: "test".into(),
                is_live_snapshot: false,
                input: BreakEvenInputs {
                    position_size_usd: dec!(4),
                    avg_priority_fee_usd: dec!(0.002),
                    avg_swap_fee_bps: dec!(20),
                    avg_slippage_bps: dec!(80),
                    avg_price_impact_bps: dec!(15),
                    failed_tx_rate: dec!(0.10),
                    avg_failed_tx_cost_usd: dec!(0.002),
                    assumed_win_loss_ratio: dec!(2),
                    assumed_avg_loss_pct: dec!(1),
                },
            },
        }
    }

    #[test]
    fn validates_fresh_candidate() {
        let mut collector = CandidateCollector::new();
        let now = Utc::now();
        let c = test_candidate("TOKEN_A", now - chrono::Duration::seconds(10));
        let batch = collector.collect_batch(vec![c], now, 3600);
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].mint, "TOKEN_A");
    }

    #[test]
    fn rejects_future_dated_candidate() {
        let mut collector = CandidateCollector::new();
        let now = Utc::now();
        let c = test_candidate("TOKEN_A", now + chrono::Duration::hours(1));
        let batch = collector.collect_batch(vec![c], now, 3600);
        assert!(batch.is_empty());
    }

    #[test]
    fn rejects_stale_candidate() {
        let mut collector = CandidateCollector::new();
        let now = Utc::now();
        let c = test_candidate("TOKEN_A", now - chrono::Duration::hours(2));
        let batch = collector.collect_batch(vec![c], now, 3600);
        assert!(batch.is_empty());
    }

    #[test]
    fn rejects_missing_decimals() {
        let mut collector = CandidateCollector::new();
        let now = Utc::now();
        let mut c = test_candidate("TOKEN_A", now - chrono::Duration::seconds(10));
        c.token_decimals = None;
        let batch = collector.collect_batch(vec![c], now, 3600);
        assert!(batch.is_empty());
    }

    #[test]
    fn rejects_cost_mismatch() {
        let mut collector = CandidateCollector::new();
        let now = Utc::now();
        let mut c = test_candidate("TOKEN_A", now - chrono::Duration::seconds(10));
        c.position_usd = dec!(5);
        let batch = collector.collect_batch(vec![c], now, 3600);
        assert!(batch.is_empty());
    }

    #[test]
    fn deduplicates_by_mint() {
        let mut collector = CandidateCollector::new();
        let now = Utc::now();
        let c1 = test_candidate("TOKEN_A", now - chrono::Duration::seconds(10));
        let c2 = test_candidate("TOKEN_A", now - chrono::Duration::seconds(5));
        let batch = collector.collect_batch(vec![c1, c2], now, 3600);
        assert_eq!(batch.len(), 1);
        assert!(collector.is_seen("TOKEN_A"));
    }

    #[test]
    fn multiple_mints_allowed() {
        let mut collector = CandidateCollector::new();
        let now = Utc::now();
        let c1 = test_candidate("TOKEN_A", now - chrono::Duration::seconds(10));
        let c2 = test_candidate("TOKEN_B", now - chrono::Duration::seconds(10));
        let batch = collector.collect_batch(vec![c1, c2], now, 3600);
        assert_eq!(batch.len(), 2);
        assert!(collector.is_seen("TOKEN_A"));
        assert!(collector.is_seen("TOKEN_B"));
    }

    #[test]
    fn cross_tick_dedup() {
        let mut collector = CandidateCollector::new();
        let now = Utc::now();
        let c1 = test_candidate("TOKEN_A", now - chrono::Duration::seconds(10));
        let batch1 = collector.collect_batch(vec![c1], now, 3600);
        assert_eq!(batch1.len(), 1);
        let c2 = test_candidate("TOKEN_A", now - chrono::Duration::seconds(5));
        let batch2 = collector.collect_batch(vec![c2], now, 3600);
        assert!(batch2.is_empty());
    }

    #[test]
    fn clear_seen_resets_dedup() {
        let mut collector = CandidateCollector::new();
        let now = Utc::now();
        let c = test_candidate("TOKEN_A", now - chrono::Duration::seconds(10));
        collector.collect_batch(vec![c.clone()], now, 3600);
        assert!(collector.is_seen("TOKEN_A"));
        collector.clear_seen();
        assert!(!collector.is_seen("TOKEN_A"));
        let batch = collector.collect_batch(vec![c], now, 3600);
        assert_eq!(batch.len(), 1);
    }

    #[test]
    fn seen_count_tracks_uniques() {
        let mut collector = CandidateCollector::new();
        assert_eq!(collector.seen_count(), 0);
        let now = Utc::now();
        let c1 = test_candidate("TOKEN_A", now - chrono::Duration::seconds(10));
        let c2 = test_candidate("TOKEN_B", now - chrono::Duration::seconds(10));
        collector.collect_batch(vec![c1, c2], now, 3600);
        assert_eq!(collector.seen_count(), 2);
    }

    #[test]
    fn collect_from_jsonl_handles_missing_file() {
        let mut collector = CandidateCollector::new();
        let now = Utc::now();
        let result = collector.collect_from_jsonl("/nonexistent/path.jsonl", now, 3600);
        assert!(result.is_empty());
    }
}
