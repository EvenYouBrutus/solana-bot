use crate::collector::swap_parser::{parse_swap_from_transaction, SwapDirection};
use crate::collector::token_data::{fetch_market_snapshot, fetch_token_safety};
use crate::config::types::Config;
use crate::data::rpc::RpcPool;
use crate::domain::wallet::{Side, WalletStats, WalletTier, WalletTradeObservation};
use crate::economics::{BreakEvenInputs, CostModel};
use crate::execution::Executor;
use crate::runtime::CandidateInput;
use crate::smart_money::{SmartMoneyThresholds, WalletTracker};
use chrono::{Duration, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

const MAX_SIGNATURES_PER_WALLET: u32 = 100;
const MAX_TRANSACTIONS_PER_TICK: usize = 10;

type SwapRecord = (
    String,        // wallet
    SwapDirection, // direction
    String,        // input_mint
    String,        // output_mint
    u64,           // input_amount
    u64,           // output_amount
    u8,            // input_decimals
    u8,            // output_decimals
    String,        // dex
    i64,           // block_time
    String,        // signature
);

#[allow(dead_code)]
struct OpenPosition {
    sol_spent: Decimal,
    tokens_received: Decimal,
    timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Clone)]
#[allow(dead_code)]
struct CompletedTrade {
    return_pct: Decimal,
    pnl_sol: Decimal,
    entry_time: chrono::DateTime<chrono::Utc>,
    exit_time: chrono::DateTime<chrono::Utc>,
}

struct WalletAccumulator {
    open_positions: HashMap<String, VecDeque<OpenPosition>>,
    completed_trades: Vec<CompletedTrade>,
}

impl WalletAccumulator {
    fn new() -> Self {
        Self {
            open_positions: HashMap::new(),
            completed_trades: Vec::new(),
        }
    }

    fn record_buy(
        &mut self,
        mint: &str,
        sol_spent: Decimal,
        tokens_received: Decimal,
        timestamp: chrono::DateTime<chrono::Utc>,
    ) {
        self.open_positions
            .entry(mint.to_string())
            .or_default()
            .push_back(OpenPosition {
                sol_spent,
                tokens_received,
                timestamp,
            });
    }

    fn record_sell(
        &mut self,
        mint: &str,
        _tokens_sold: Decimal,
        sol_received: Decimal,
        sell_time: chrono::DateTime<chrono::Utc>,
    ) -> Option<CompletedTrade> {
        let queue = self.open_positions.get_mut(mint)?;
        if queue.is_empty() {
            return None;
        }
        let entry = queue.pop_front()?;
        let return_pct = if entry.sol_spent > Decimal::ZERO {
            ((sol_received - entry.sol_spent) / entry.sol_spent * dec!(100)).round_dp(2)
        } else {
            Decimal::ZERO
        };
        let pnl_sol = sol_received - entry.sol_spent;
        let trade = CompletedTrade {
            return_pct,
            pnl_sol,
            entry_time: entry.timestamp,
            exit_time: sell_time,
        };
        self.completed_trades.push(trade.clone());
        Some(trade)
    }

    fn build_stats(&self, wallet: &str) -> WalletStats {
        let trades = self.completed_trades.len() as u32;
        if trades == 0 {
            return WalletStats {
                wallet: wallet.to_string(),
                entity_id: None,
                realized_pnl_usd: Decimal::ZERO,
                win_rate: Decimal::ZERO,
                avg_return_pct: Decimal::ZERO,
                median_return_pct: Decimal::ZERO,
                max_drawdown_pct: Decimal::ZERO,
                trades: 0,
                recent_return_pct: Decimal::ZERO,
                concentration_pct: Decimal::ZERO,
                scam_exposure_pct: Decimal::ZERO,
                score: Decimal::ZERO,
                tier: WalletTier::Candidate,
                updated_at: Utc::now(),
            };
        }

        let wins = self
            .completed_trades
            .iter()
            .filter(|t| t.return_pct > Decimal::ZERO)
            .count() as u32;
        let win_rate = Decimal::from(wins) / Decimal::from(trades);

        let returns: Vec<Decimal> = self.completed_trades.iter().map(|t| t.return_pct).collect();
        let avg_return = returns.iter().sum::<Decimal>() / Decimal::from(trades);

        let mut sorted_returns = returns.clone();
        sorted_returns.sort();
        let median_return = sorted_returns[sorted_returns.len() / 2];

        let recent_return = self
            .completed_trades
            .last()
            .map(|t| t.return_pct)
            .unwrap_or_default();

        let realized_pnl: Decimal = self.completed_trades.iter().map(|t| t.pnl_sol).sum();

        let mut peak = Decimal::ZERO;
        let mut max_dd = Decimal::ZERO;
        let mut cumulative = Decimal::ZERO;
        for t in &self.completed_trades {
            cumulative += t.pnl_sol;
            if cumulative > peak {
                peak = cumulative;
            }
            let dd = if peak > Decimal::ZERO {
                (peak - cumulative) / peak * dec!(100)
            } else {
                Decimal::ZERO
            };
            if dd > max_dd {
                max_dd = dd;
            }
        }

        WalletStats {
            wallet: wallet.to_string(),
            entity_id: None,
            realized_pnl_usd: realized_pnl,
            win_rate,
            avg_return_pct: avg_return,
            median_return_pct: median_return,
            max_drawdown_pct: max_dd,
            trades,
            recent_return_pct: recent_return,
            concentration_pct: Decimal::ZERO,
            scam_exposure_pct: Decimal::ZERO,
            score: Decimal::ZERO,
            tier: WalletTier::Candidate,
            updated_at: Utc::now(),
        }
    }
}

pub struct WalletMonitor {
    rpc: Arc<RpcPool>,
    executor: Arc<dyn Executor>,
    config: Arc<Config>,
    wallets: Vec<String>,
    processed_sigs: HashMap<String, HashSet<String>>,
    accumulators: HashMap<String, WalletAccumulator>,
    wallet_tracker: WalletTracker,
    seen_mints: HashSet<String>,
    offered_mints: HashSet<String>,
    position_usd: Decimal,
    consensus_window_secs: u64,
}

impl WalletMonitor {
    pub async fn new(
        config: std::sync::Arc<Config>,
        rpc: std::sync::Arc<RpcPool>,
        executor: std::sync::Arc<dyn Executor>,
    ) -> Result<Self, anyhow::Error> {
        let wallets = load_wallets(&config.wallet_monitor.wallets_file)?;
        if wallets.is_empty() {
            tracing::warn!(
                "no valid wallets found in {}",
                config.wallet_monitor.wallets_file
            );
        } else {
            tracing::info!(count = wallets.len(), "loaded monitored wallets");
        }
        let position_usd = config.wallet_monitor.position_usd;
        let consensus_window_secs = config.wallet_monitor.consensus_window_secs;

        let mut monitor = Self {
            rpc,
            executor,
            config,
            wallets,
            processed_sigs: HashMap::new(),
            accumulators: HashMap::new(),
            wallet_tracker: WalletTracker::new(SmartMoneyThresholds::default()),
            seen_mints: HashSet::new(),
            offered_mints: HashSet::new(),
            position_usd,
            consensus_window_secs,
        };

        monitor.rebuild_all_wallet_stats().await;
        Ok(monitor)
    }

    async fn rebuild_all_wallet_stats(&mut self) {
        for wallet in &self.wallets.clone() {
            if let Err(e) = self.rebuild_wallet_history(wallet).await {
                tracing::warn!(
                    wallet = %wallet,
                    error = %e,
                    "failed to rebuild wallet history"
                );
            }
        }
    }

    async fn rebuild_wallet_history(&mut self, wallet: &str) -> Result<(), anyhow::Error> {
        tracing::info!(wallet = %wallet, "rebuilding wallet history from RPC");

        let sigs: Vec<crate::data::rpc::SignatureEntry> = self
            .rpc
            .signatures_for_address(wallet, MAX_SIGNATURES_PER_WALLET)
            .await
            .map_err(|e| anyhow::anyhow!("signatures RPC failed: {e}"))?;

        tracing::info!(
            wallet = %wallet,
            signatures = sigs.len(),
            "wallet history signatures fetched"
        );

        let mut processed = HashSet::new();
        let mut accumulator = WalletAccumulator::new();
        let sol_price = self.config.economics.sol_price_usd.unwrap_or(dec!(150));
        let mut parsed = 0u32;
        let mut skipped = 0u32;

        for sig in &sigs {
            if sig.err.is_some() {
                continue;
            }
            processed.insert(sig.signature.clone());

            let tx = match self.rpc.transaction(&sig.signature).await {
                Ok(Some(t)) => t,
                _ => {
                    skipped += 1;
                    continue;
                }
            };

            if let Some(swap) = parse_swap_from_transaction(&tx, wallet) {
                parsed += 1;
                let ts =
                    chrono::DateTime::from_timestamp(swap.block_time, 0).unwrap_or_else(Utc::now);

                let input_sol = Decimal::from(swap.input_amount)
                    / Decimal::from(10u64.pow(swap.input_decimals as u32));
                let output_tokens = Decimal::from(swap.output_amount);

                match swap.direction {
                    SwapDirection::Buy => {
                        self.wallet_tracker.observe(WalletTradeObservation {
                            wallet: wallet.to_string(),
                            mint: swap.output_mint.clone(),
                            side: Side::Buy,
                            notional_usd: input_sol * sol_price,
                            observed_at: ts,
                            received_at: Utc::now(),
                            signature: swap.signature.clone(),
                        });
                        accumulator.record_buy(&swap.output_mint, input_sol, output_tokens, ts);
                        self.seen_mints.insert(swap.output_mint);
                    }
                    SwapDirection::Sell => {
                        let sol_out = Decimal::from(swap.output_amount)
                            / Decimal::from(10u64.pow(swap.output_decimals as u32));
                        self.wallet_tracker.observe(WalletTradeObservation {
                            wallet: wallet.to_string(),
                            mint: swap.input_mint.clone(),
                            side: Side::Sell,
                            notional_usd: sol_out * sol_price,
                            observed_at: ts,
                            received_at: Utc::now(),
                            signature: swap.signature.clone(),
                        });
                        accumulator.record_sell(&swap.input_mint, output_tokens, sol_out, ts);
                    }
                }
            }
        }

        let stats = accumulator.build_stats(wallet);
        self.accumulators.insert(wallet.to_string(), accumulator);
        self.processed_sigs
            .insert(wallet.to_string(), processed.clone());

        tracing::info!(
            wallet = %wallet,
            signatures = sigs.len(),
            parsed_swaps = parsed,
            rpc_skipped = skipped,
            completed_trades = stats.trades,
            "wallet history rebuilt"
        );

        if stats.trades > 0 {
            self.wallet_tracker.upsert(stats);
        }

        Ok(())
    }

    pub async fn tick(&mut self) -> Result<Vec<CandidateInput>, anyhow::Error> {
        let mut new_candidates = Vec::new();
        let now = Utc::now();

        tracing::info!(wallets = self.wallets.len(), "wallet polling started");

        for wallet in self.wallets.clone() {
            if let Err(e) = self.poll_wallet(&wallet, &mut new_candidates, now).await {
                tracing::debug!(
                    wallet = %wallet,
                    error = %e,
                    "wallet poll failed this tick"
                );
            }
        }

        tracing::info!(
            candidates = new_candidates.len(),
            "wallet monitor tick complete"
        );

        Ok(new_candidates)
    }

    async fn poll_wallet(
        &mut self,
        wallet: &str,
        new_candidates: &mut Vec<CandidateInput>,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), anyhow::Error> {
        tracing::info!(wallet = %wallet, "polling wallet for new signatures");

        let sigs: Vec<crate::data::rpc::SignatureEntry> = self
            .rpc
            .signatures_for_address(wallet, 50)
            .await
            .map_err(|e| anyhow::anyhow!("signatures RPC: {e}"))?;

        tracing::info!(
            wallet = %wallet,
            signatures = sigs.len(),
            "signatures fetched"
        );

        if sigs.is_empty() {
            tracing::info!(
                wallet = %wallet,
                "no signatures found for wallet; wallet may have no recent activity"
            );
        }

        let sol_price = self.config.economics.sol_price_usd.unwrap_or(dec!(150));

        let mut tx_count = 0usize;
        let mut swaps_this_tick: Vec<SwapRecord> = Vec::new();
        let mut new_mints_this_tick: HashSet<String> = HashSet::new();

        for sig in &sigs {
            if sig.err.is_some() {
                continue;
            }
            let already_processed = self
                .processed_sigs
                .get(wallet)
                .map(|s| s.contains(&sig.signature))
                .unwrap_or(false);
            if already_processed {
                continue;
            }
            if tx_count >= MAX_TRANSACTIONS_PER_TICK {
                break;
            }

            let tx = match self.rpc.transaction(&sig.signature).await {
                Ok(Some(t)) => t,
                _ => continue,
            };
            tx_count += 1;
            self.processed_sigs
                .entry(wallet.to_string())
                .or_default()
                .insert(sig.signature.clone());

            if let Some(swap) = parse_swap_from_transaction(&tx, wallet) {
                swaps_this_tick.push((
                    swap.wallet.clone(),
                    swap.direction.clone(),
                    swap.input_mint.clone(),
                    swap.output_mint.clone(),
                    swap.input_amount,
                    swap.output_amount,
                    swap.input_decimals,
                    swap.output_decimals,
                    swap.dex.clone(),
                    swap.block_time,
                    swap.signature.clone(),
                ));
                match swap.direction {
                    SwapDirection::Buy => {
                        new_mints_this_tick.insert(swap.output_mint.clone());
                        self.seen_mints.insert(swap.output_mint.clone());
                    }
                    SwapDirection::Sell => {}
                }
            }
        }

        tracing::info!(
            wallet = %wallet,
            new_swaps = swaps_this_tick.len(),
            new_mints = new_mints_this_tick.len(),
            transactions_fetched = tx_count,
            "swaps parsed from wallet"
        );

        for (
            ref _wallet_addr,
            direction,
            input_mint,
            output_mint,
            input_amount,
            output_amount,
            input_decimals,
            output_decimals,
            _dex,
            block_time,
            ref signature,
        ) in swaps_this_tick
        {
            let ts = chrono::DateTime::from_timestamp(block_time, 0).unwrap_or(now);
            let input_sol =
                Decimal::from(input_amount) / Decimal::from(10u64.pow(input_decimals as u32));
            let output_tokens = Decimal::from(output_amount);

            match direction {
                SwapDirection::Buy => {
                    self.wallet_tracker.observe(WalletTradeObservation {
                        wallet: wallet.to_string(),
                        mint: output_mint.clone(),
                        side: Side::Buy,
                        notional_usd: input_sol * sol_price,
                        observed_at: ts,
                        received_at: now,
                        signature: signature.clone(),
                    });
                    self.accumulators
                        .entry(wallet.to_string())
                        .or_insert_with(WalletAccumulator::new)
                        .record_buy(&output_mint, input_sol, output_tokens, ts);
                }
                SwapDirection::Sell => {
                    let sol_out = Decimal::from(output_amount)
                        / Decimal::from(10u64.pow(output_decimals as u32));
                    self.wallet_tracker.observe(WalletTradeObservation {
                        wallet: wallet.to_string(),
                        mint: input_mint.clone(),
                        side: Side::Sell,
                        notional_usd: sol_out * sol_price,
                        observed_at: ts,
                        received_at: now,
                        signature: signature.clone(),
                    });
                    self.accumulators
                        .entry(wallet.to_string())
                        .or_insert_with(WalletAccumulator::new)
                        .record_sell(&input_mint, output_tokens, sol_out, ts);
                }
            }
        }

        let stats = self
            .accumulators
            .entry(wallet.to_string())
            .or_insert_with(WalletAccumulator::new)
            .build_stats(wallet);
        if stats.trades > 0 {
            self.wallet_tracker.upsert(stats);
        }

        for mint in &new_mints_this_tick {
            if self.offered_mints.contains(mint) {
                tracing::debug!(
                    mint = %mint,
                    "mint already offered as candidate; skipping"
                );
                continue;
            }
            self.check_and_build_candidate(mint, new_candidates, now)
                .await;
        }

        Ok(())
    }

    async fn check_and_build_candidate(
        &mut self,
        mint: &str,
        new_candidates: &mut Vec<CandidateInput>,
        now: chrono::DateTime<chrono::Utc>,
    ) {
        if self.seen_mints.contains(mint) && new_candidates.iter().any(|c| c.mint == mint) {
            return;
        }

        let consensus_wallets = self.wallet_tracker.qualified_consensus_at(
            mint,
            now,
            Duration::seconds(self.consensus_window_secs as i64),
        );

        if consensus_wallets.len() < self.config.strategy.min_consensus_wallets {
            tracing::debug!(
                mint = %mint,
                wallets = consensus_wallets.len(),
                required = self.config.strategy.min_consensus_wallets,
                "insufficient consensus wallets for candidate"
            );
            return;
        }

        tracing::info!(
            mint = %mint,
            wallets = consensus_wallets.len(),
            "consensus detected for token"
        );

        let safety = match fetch_token_safety(
            &self.rpc,
            mint,
            self.config.strategy.min_token_age_secs,
        )
        .await
        {
            Ok(Some(s)) => s,
            Ok(None) => {
                tracing::info!(mint = %mint, "token safety data unavailable; candidate rejected");
                return;
            }
            Err(e) => {
                tracing::info!(mint = %mint, error = %e, "token safety fetch failed; candidate rejected");
                return;
            }
        };

        let token_decimals = match self.rpc.mint_account_info(mint).await {
            Ok(Some(info)) => info.decimals,
            _ => 6,
        };

        let sol_price = self.config.economics.sol_price_usd.unwrap_or(dec!(150));
        let base_mint_decimals = 9u8;
        let input_amount = (self.position_usd / sol_price * dec!(1_000_000_000))
            .to_string()
            .parse::<u64>()
            .unwrap_or(4_000_000);

        let market = match fetch_market_snapshot(
            &self.rpc,
            self.executor.as_ref(),
            mint,
            sol_price,
            base_mint_decimals,
            input_amount,
            self.config.execution.slippage_bps,
        )
        .await
        {
            Ok(Some(m)) => m,
            Ok(None) => {
                tracing::info!(mint = %mint, "market snapshot unavailable; candidate rejected");
                return;
            }
            Err(e) => {
                tracing::info!(mint = %mint, error = %e, "market snapshot fetch failed; candidate rejected");
                return;
            }
        };

        let avg_return: Decimal = consensus_wallets
            .iter()
            .map(|w| w.avg_return_pct)
            .sum::<Decimal>()
            / Decimal::from(consensus_wallets.len());
        let expected_gross_return = avg_return.max(dec!(5));

        let cost_model = CostModel {
            observed_at: now,
            input: BreakEvenInputs {
                position_size_usd: self.position_usd,
                avg_priority_fee_usd: dec!(0.0004),
                avg_swap_fee_bps: dec!(30),
                avg_slippage_bps: dec!(50),
                avg_price_impact_bps: dec!(20),
                failed_tx_rate: dec!(0.05),
                avg_failed_tx_cost_usd: dec!(0.002),
                assumed_win_loss_ratio: dec!(2),
                assumed_avg_loss_pct: dec!(10),
            },
            source: "wallet_monitor".into(),
            is_live_snapshot: true,
        };

        let input_lamports = input_amount;

        let candidate = CandidateInput {
            mint: mint.to_string(),
            token_decimals: Some(token_decimals),
            base_mint_decimals: Some(base_mint_decimals),
            input_amount: input_lamports,
            position_usd: self.position_usd,
            expected_gross_return_pct: expected_gross_return,
            market,
            safety,
            wallets: consensus_wallets.into_iter().cloned().collect(),
            costs: cost_model,
        };

        tracing::info!(
            mint = %mint,
            position_usd = %self.position_usd,
            "candidate created"
        );

        self.seen_mints.insert(mint.to_string());
        self.offered_mints.insert(mint.to_string());
        new_candidates.push(candidate);
    }
}

fn load_wallets(path: &str) -> Result<Vec<String>, anyhow::Error> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("cannot read wallets file {path}: {e}"))?;
    let wallets: Vec<String> = content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter(|l| is_valid_solana_address(l))
        .map(String::from)
        .collect();
    Ok(wallets)
}

fn is_valid_solana_address(addr: &str) -> bool {
    if addr.len() < 32 || addr.len() > 44 {
        return false;
    }
    addr.chars()
        .all(|c| matches!(c, '1'..='9' | 'A'..='H' | 'J'..='N' | 'P'..='Z' | 'a'..='k' | 'm'..='z'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_wallets_parses_file() {
        let dir = std::env::temp_dir().join("wallet_test_load");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("wallets.txt");
        std::fs::write(
            &path,
            "# comment\n5kqEvH3gnx5HUYA8UmK3Za5gF3kRpY3oUg3TCY4tJhPb\n\ninvalid\n",
        )
        .unwrap();
        let wallets = load_wallets(path.to_str().unwrap()).unwrap();
        assert_eq!(wallets.len(), 1);
        assert_eq!(wallets[0], "5kqEvH3gnx5HUYA8UmK3Za5gF3kRpY3oUg3TCY4tJhPb");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wallet_accumulator_fifomatching() {
        let mut acc = WalletAccumulator::new();
        acc.record_buy("TOKEN", dec!(1), dec!(100), Utc::now());
        acc.record_buy("TOKEN", dec!(2), dec!(200), Utc::now());
        let trade = acc
            .record_sell("TOKEN", dec!(100), dec!(1.5), Utc::now())
            .unwrap();
        assert!(trade.return_pct > Decimal::ZERO);
        assert_eq!(acc.completed_trades.len(), 1);
    }
}
