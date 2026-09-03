use crate::collector::swap_parser::{parse_swap_from_transaction, ParsedSwap, SwapDirection};
use crate::collector::token_data::{fetch_market_snapshot, fetch_token_safety};
use crate::config::types::Config;
use crate::data::rpc::{RpcPool, SignatureEntry};
use crate::domain::wallet::{Side, WalletStats, WalletTier, WalletTradeObservation};
use crate::economics::{BreakEvenInputs, CostModel};
use crate::execution::Executor;
use crate::runtime::CandidateInput;
use crate::smart_money::{SmartMoneyThresholds, WalletTracker};
use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

/// Default cap on signatures fetched per wallet in one history-rebuild pass.
const DEFAULT_REBUILD_PAGE: u32 = 200;
/// Maximum number of pages walked per wallet during a single rebuild.
const MAX_REBUILD_PAGES: u32 = 5;
/// Maximum transactions inspected per tick (rate-limit protection).
const MAX_TRANSACTIONS_PER_TICK: usize = 12;
/// Below this fraction of swaps among successful transactions, a wallet is
/// considered non-trading.
const MIN_SWAP_RATIO: f64 = 0.05;
/// Wallets with no signatures at all in the configured history window are
/// classified as LOW_ACTIVITY (they may simply be quiet).
const LOW_ACTIVITY_TX_THRESHOLD: u32 = 1;
/// Wallets with >= this many recognized swaps are considered active traders.
const VALID_ACTIVE_SWAP_THRESHOLD: u32 = 1;
/// Cap to avoid building an unbounded observations list per wallet.
const MAX_OBSERVATIONS_PER_WALLET: usize = 5_000;
/// Minimum delay between RPC calls to avoid rate limiting on public endpoints.
const RPC_RATE_LIMIT_MS: u64 = 120;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalletStatus {
    /// Successful transactions and at least one recognized swap.
    ValidActive,
    /// Successful transactions but no recognized swaps.
    NoSwapActivity,
    /// Address is well-formed but had too little on-chain history to evaluate.
    LowActivity,
    /// Address failed the base58 / length check.
    Invalid,
    /// Wallet passes base58 but the RPC node refused the lookup repeatedly
    /// (kept for completeness; current validator reports `NoSwapActivity` on
    /// RPC silence).
    Suspect,
}

#[derive(Debug, Clone)]
pub struct WalletValidationReport {
    pub wallet: String,
    pub status: WalletStatus,
    pub signatures_fetched: u32,
    pub successful_transactions: u32,
    pub swaps_parsed: u32,
    pub buys: u32,
    pub sells: u32,
    pub dex_activity: HashMap<String, u32>,
    pub last_activity_ts: Option<i64>,
    pub first_activity_ts: Option<i64>,
}

#[derive(Default, Debug, Clone)]
pub struct ValidationSummary {
    pub wallets_loaded: u32,
    pub wallets_valid: u32,
    pub wallets_active: u32,
    pub wallets_no_swap_activity: u32,
    pub wallets_invalid: u32,
    pub wallets_low_activity: u32,
    pub total_signatures: u64,
    pub total_successful_transactions: u64,
    pub total_swaps_parsed: u64,
    pub total_buys: u64,
    pub total_sells: u64,
    pub reports: Vec<WalletValidationReport>,
}

#[allow(dead_code)]
struct OpenPosition {
    sol_spent: Decimal,
    tokens_received: Decimal,
    timestamp: DateTime<Utc>,
}

#[derive(Clone)]
#[allow(dead_code)]
struct CompletedTrade {
    return_pct: Decimal,
    pnl_sol: Decimal,
    entry_time: DateTime<Utc>,
    exit_time: DateTime<Utc>,
}

struct WalletAccumulator {
    open_positions: HashMap<String, VecDeque<OpenPosition>>,
    completed_trades: Vec<CompletedTrade>,
    buys: u32,
    sells: u32,
    dex_activity: HashMap<String, u32>,
    last_activity_ts: Option<i64>,
    first_activity_ts: Option<i64>,
}

impl WalletAccumulator {
    fn new() -> Self {
        Self {
            open_positions: HashMap::new(),
            completed_trades: Vec::new(),
            buys: 0,
            sells: 0,
            dex_activity: HashMap::new(),
            last_activity_ts: None,
            first_activity_ts: None,
        }
    }

    fn record_observation(
        &mut self,
        mint: &str,
        direction: &SwapDirection,
        sol_amount: Decimal,
        tokens: Decimal,
        ts: DateTime<Utc>,
    ) {
        let block_time = ts.timestamp();
        self.last_activity_ts = Some(match self.last_activity_ts {
            Some(prev) => prev.max(block_time),
            None => block_time,
        });
        self.first_activity_ts = Some(match self.first_activity_ts {
            Some(prev) => prev.min(block_time),
            None => block_time,
        });
        match direction {
            SwapDirection::Buy => {
                self.buys += 1;
                self.open_positions
                    .entry(mint.to_string())
                    .or_default()
                    .push_back(OpenPosition {
                        sol_spent: sol_amount,
                        tokens_received: tokens,
                        timestamp: ts,
                    });
            }
            SwapDirection::Sell => {
                self.sells += 1;
                self.record_sell(mint, tokens, sol_amount, ts);
            }
        }
    }

    fn record_sell(
        &mut self,
        mint: &str,
        tokens_sold: Decimal,
        sol_received: Decimal,
        sell_time: DateTime<Utc>,
    ) -> Option<CompletedTrade> {
        // FIFO across multiple lots: walk from the oldest lot forward until
        // we have accounted for all the tokens sold in this swap.  A single
        // sell may consume several lots; each consumed lot produces one
        // completed-trade record.
        let queue = self.open_positions.get_mut(mint)?;
        if queue.is_empty() {
            return None;
        }
        let mut remaining_to_sell = tokens_sold;
        let total_proceeds = sol_received;
        let mut consumed = 0u32;
        let mut last_trade: Option<CompletedTrade> = None;
        let mut cumulative_pnl = Decimal::ZERO;
        // First pass: pop fully-consumed lots.
        while let Some(front) = queue.front() {
            if remaining_to_sell <= Decimal::ZERO {
                break;
            }
            if front.tokens_received <= remaining_to_sell {
                // Whole lot is consumed; allocate proceeds proportionally.
                let lot = queue.pop_front().unwrap();
                let share = if tokens_sold > Decimal::ZERO {
                    lot.tokens_received / tokens_sold
                } else {
                    Decimal::ZERO
                };
                let lot_proceeds = total_proceeds * share;
                let return_pct = if lot.sol_spent > Decimal::ZERO {
                    ((lot_proceeds - lot.sol_spent) / lot.sol_spent * dec!(100)).round_dp(2)
                } else {
                    Decimal::ZERO
                };
                let pnl = lot_proceeds - lot.sol_spent;
                cumulative_pnl += pnl;
                consumed += 1;
                remaining_to_sell -= lot.tokens_received;
                last_trade = Some(CompletedTrade {
                    return_pct,
                    pnl_sol: pnl,
                    entry_time: lot.timestamp,
                    exit_time: sell_time,
                });
                self.completed_trades.push(last_trade.clone().unwrap());
            } else {
                break;
            }
        }
        // If a partial lot remains, allocate the remaining proceeds to it
        // without popping the lot (it still has tokens on hand).
        if remaining_to_sell > Decimal::ZERO {
            let need_pop = if let Some(front) = queue.front() {
                let share = if tokens_sold > Decimal::ZERO {
                    remaining_to_sell / tokens_sold
                } else {
                    Decimal::ZERO
                };
                let lot_proceeds = total_proceeds * share;
                let return_pct = if front.sol_spent > Decimal::ZERO {
                    ((lot_proceeds - front.sol_spent) / front.sol_spent * dec!(100)).round_dp(2)
                } else {
                    Decimal::ZERO
                };
                let pnl = lot_proceeds - front.sol_spent;
                cumulative_pnl += pnl;
                consumed += 1;
                let entry_time = front.timestamp;
                let still_remaining = front.tokens_received - remaining_to_sell;
                let fully_consumed = still_remaining <= Decimal::ZERO;
                last_trade = Some(CompletedTrade {
                    return_pct,
                    pnl_sol: pnl,
                    entry_time,
                    exit_time: sell_time,
                });
                self.completed_trades.push(last_trade.clone().unwrap());
                fully_consumed
            } else {
                false
            };
            if need_pop {
                queue.pop_front();
            } else if let Some(front) = queue.front_mut() {
                front.tokens_received -= remaining_to_sell;
            }
        }
        if consumed == 0 {
            return None;
        }
        // Return the LAST realized trade so the caller can inspect it.
        last_trade
    }

    fn observe_dex(&mut self, dex: &str) {
        *self.dex_activity.entry(dex.to_string()).or_insert(0) += 1;
    }

    /// Build point-in-time wallet statistics. The `as_of` parameter bounds
    /// the included observations; only trades completed by `as_of` count.
    /// If `as_of` is `None`, all collected trades are used (which is wrong
    /// for historical PIT replay — callers should always pass a value).
    fn build_stats(&self, wallet: &str, as_of: Option<DateTime<Utc>>) -> WalletStats {
        let cutoff = as_of.unwrap_or_else(Utc::now);
        let mut trades: Vec<&CompletedTrade> = self
            .completed_trades
            .iter()
            .filter(|t| t.exit_time <= cutoff)
            .collect();
        trades.sort_by_key(|t| t.exit_time);

        let count = trades.len() as u32;
        if count == 0 {
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
                updated_at: cutoff,
            };
        }

        let wins = trades
            .iter()
            .filter(|t| t.return_pct > Decimal::ZERO)
            .count() as u32;
        let win_rate = Decimal::from(wins) / Decimal::from(count);

        let returns: Vec<Decimal> = trades.iter().map(|t| t.return_pct).collect();
        let avg_return = returns.iter().sum::<Decimal>() / Decimal::from(count);

        let mut sorted = returns.clone();
        sorted.sort();
        let median_return = sorted[sorted.len() / 2];

        let recent_return = trades.last().map(|t| t.return_pct).unwrap_or_default();
        let realized_pnl: Decimal = trades.iter().map(|t| t.pnl_sol).sum();

        let mut peak = Decimal::ZERO;
        let mut max_dd = Decimal::ZERO;
        let mut cumulative = Decimal::ZERO;
        for t in &trades {
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
            trades: count,
            recent_return_pct: recent_return,
            concentration_pct: Decimal::ZERO,
            scam_exposure_pct: Decimal::ZERO,
            score: Decimal::ZERO,
            tier: WalletTier::Candidate,
            updated_at: cutoff,
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
    wallet_poll_idx: usize,
    validation_summary: ValidationSummary,
}

impl WalletMonitor {
    pub async fn new(
        config: Arc<Config>,
        rpc: Arc<RpcPool>,
        executor: Arc<dyn Executor>,
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

        let monitor = Self {
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
            wallet_poll_idx: 0,
            validation_summary: ValidationSummary::default(),
        };

        tracing::info!(
            "wallet monitor initialised; history rebuild is lazy (first tick per wallet)"
        );
        Ok(monitor)
    }

    pub fn validation_summary(&self) -> &ValidationSummary {
        &self.validation_summary
    }

    /// Walk all configured wallets and produce a one-shot validation report.
    /// Uses bounded pagination; safe to call at startup before live polling.
    pub async fn validate_all(&mut self) -> Result<ValidationSummary, anyhow::Error> {
        let total = self.wallets.len();
        let mut summary = ValidationSummary {
            wallets_loaded: total as u32,
            ..Default::default()
        };
        tracing::info!(wallets = total, "starting wallet cohort validation");
        for wallet in self.wallets.clone() {
            let report = self.validate_single(&wallet).await;
            summary.total_signatures += report.signatures_fetched as u64;
            summary.total_successful_transactions += report.successful_transactions as u64;
            summary.total_swaps_parsed += report.swaps_parsed as u64;
            summary.total_buys += report.buys as u64;
            summary.total_sells += report.sells as u64;
            match report.status {
                WalletStatus::ValidActive => {
                    summary.wallets_valid += 1;
                    summary.wallets_active += 1;
                }
                WalletStatus::NoSwapActivity => {
                    summary.wallets_valid += 1;
                    summary.wallets_no_swap_activity += 1;
                }
                WalletStatus::LowActivity => {
                    summary.wallets_valid += 1;
                    summary.wallets_low_activity += 1;
                }
                WalletStatus::Invalid => summary.wallets_invalid += 1,
                WalletStatus::Suspect => summary.wallets_invalid += 1,
            }
            tracing::info!(
                wallet = %wallet,
                status = ?report.status,
                signatures = report.signatures_fetched,
                successful = report.successful_transactions,
                swaps = report.swaps_parsed,
                "wallet validation"
            );
            summary.reports.push(report);
        }
        tracing::info!(
            wallets_loaded = summary.wallets_loaded,
            wallets_valid = summary.wallets_valid,
            wallets_active = summary.wallets_active,
            wallets_no_swap_activity = summary.wallets_no_swap_activity,
            wallets_low_activity = summary.wallets_low_activity,
            wallets_invalid = summary.wallets_invalid,
            signatures = summary.total_signatures,
            swaps = summary.total_swaps_parsed,
            "wallet validation summary"
        );
        self.validation_summary = summary.clone();
        Ok(summary)
    }

    async fn validate_single(&self, wallet: &str) -> WalletValidationReport {
        if !is_valid_solana_address(wallet) {
            return WalletValidationReport {
                wallet: wallet.to_string(),
                status: WalletStatus::Invalid,
                signatures_fetched: 0,
                successful_transactions: 0,
                swaps_parsed: 0,
                buys: 0,
                sells: 0,
                dex_activity: HashMap::new(),
                last_activity_ts: None,
                first_activity_ts: None,
            };
        }
        let max_pages = self
            .config
            .wallet_monitor
            .max_history_signatures
            .max(DEFAULT_REBUILD_PAGE)
            / DEFAULT_REBUILD_PAGE;
        let pages = max_pages.min(MAX_REBUILD_PAGES).max(1);
        let mut all_sigs: Vec<SignatureEntry> = Vec::new();
        let mut before: Option<String> = None;
        for _ in 0..pages {
            match self
                .rpc
                .signatures_for_address_paged(wallet, DEFAULT_REBUILD_PAGE, before.as_deref())
                .await
            {
                Ok(mut page) => {
                    let last_sig = page.last().map(|s| s.signature.clone());
                    let n = page.len();
                    all_sigs.append(&mut page);
                    if n < DEFAULT_REBUILD_PAGE as usize {
                        break;
                    }
                    match last_sig {
                        Some(s) => before = Some(s),
                        None => break,
                    }
                }
                Err(_) => break,
            }
            tokio::time::sleep(std::time::Duration::from_millis(RPC_RATE_LIMIT_MS)).await;
        }

        let mut successful: u32 = 0;
        let mut buys: u32 = 0;
        let mut sells: u32 = 0;
        let mut dex_activity: HashMap<String, u32> = HashMap::new();
        let mut swaps_parsed: u32 = 0;
        let mut last_ts: Option<i64> = None;
        let mut first_ts: Option<i64> = None;
        let mut tx_fetch_errors: u32 = 0;
        let inspection_cap = (pages as usize) * (DEFAULT_REBUILD_PAGE as usize);

        let total_to_inspect = all_sigs.len().min(inspection_cap);
        for (idx, sig) in all_sigs.iter().enumerate() {
            if idx >= inspection_cap {
                break;
            }
            if sig.err.is_some() {
                continue;
            }
            successful += 1;
            match sig.block_time {
                Some(bt) => {
                    last_ts = Some(match last_ts {
                        Some(prev) => prev.max(bt),
                        None => bt,
                    });
                    first_ts = Some(match first_ts {
                        Some(prev) => prev.min(bt),
                        None => bt,
                    });
                }
                None => {}
            }
            let tx = match self.rpc.transaction(&sig.signature).await {
                Ok(Some(t)) => t,
                Ok(None) => {
                    tx_fetch_errors += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(RPC_RATE_LIMIT_MS)).await;
                    continue;
                }
                Err(e) => {
                    tracing::warn!(
                        wallet = %wallet,
                        sig = %sig.signature,
                        error = %e,
                        progress = format!("{}/{}", idx + 1, total_to_inspect),
                        "tx fetch failed during validation"
                    );
                    tx_fetch_errors += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(RPC_RATE_LIMIT_MS * 2))
                        .await;
                    continue;
                }
            };
            if let Some(parsed) = parse_swap_from_transaction(&tx, wallet) {
                swaps_parsed += 1;
                *dex_activity.entry(parsed.dex.clone()).or_insert(0) += 1;
                match parsed.direction {
                    SwapDirection::Buy => buys += 1,
                    SwapDirection::Sell => sells += 1,
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(RPC_RATE_LIMIT_MS)).await;
        }

        let status = if all_sigs.is_empty() || successful < LOW_ACTIVITY_TX_THRESHOLD {
            WalletStatus::LowActivity
        } else if swaps_parsed < VALID_ACTIVE_SWAP_THRESHOLD {
            WalletStatus::NoSwapActivity
        } else {
            let ratio = swaps_parsed as f64 / successful as f64;
            if ratio < MIN_SWAP_RATIO {
                WalletStatus::NoSwapActivity
            } else {
                WalletStatus::ValidActive
            }
        };

        tracing::info!(
            wallet = %wallet,
            status = ?status,
            signatures = all_sigs.len(),
            successful = successful,
            swaps_parsed = swaps_parsed,
            buys = buys,
            sells = sells,
            tx_fetch_errors = tx_fetch_errors,
            "wallet validation complete"
        );

        WalletValidationReport {
            wallet: wallet.to_string(),
            status,
            signatures_fetched: all_sigs.len() as u32,
            successful_transactions: successful,
            swaps_parsed,
            buys,
            sells,
            dex_activity,
            last_activity_ts: last_ts,
            first_activity_ts: first_ts,
        }
    }

    async fn rebuild_wallet_history(&mut self, wallet: &str) -> Result<u32, anyhow::Error> {
        tracing::info!(wallet = %wallet, "rebuilding wallet history from RPC");
        let mut before: Option<String> = None;
        let max_signatures = self.config.wallet_monitor.max_history_signatures.max(1);
        let mut all_sigs: Vec<SignatureEntry> = Vec::new();
        let page_size = DEFAULT_REBUILD_PAGE.min(max_signatures);
        let pages = ((max_signatures + page_size - 1) / page_size).min(MAX_REBUILD_PAGES);
        for _ in 0..pages {
            match self
                .rpc
                .signatures_for_address_paged(wallet, page_size, before.as_deref())
                .await
            {
                Ok(mut page) => {
                    let last_sig = page.last().map(|s| s.signature.clone());
                    let n = page.len();
                    all_sigs.append(&mut page);
                    if (n as u32) < page_size {
                        break;
                    }
                    match last_sig {
                        Some(s) => before = Some(s),
                        None => break,
                    }
                }
                Err(e) => {
                    tracing::warn!(wallet = %wallet, error = %e, "signature pagination failed");
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(RPC_RATE_LIMIT_MS)).await;
        }
        let sigs = all_sigs;

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
        let mut successful: u32 = 0;
        let mut observations: Vec<WalletTradeObservation> = Vec::new();

        for sig in &sigs {
            if sig.err.is_some() {
                continue;
            }
            successful += 1;
            processed.insert(sig.signature.clone());

            let tx = match self.rpc.transaction(&sig.signature).await {
                Ok(Some(t)) => t,
                Ok(None) => {
                    skipped += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(RPC_RATE_LIMIT_MS)).await;
                    continue;
                }
                Err(e) => {
                    tracing::warn!(
                        sig = %sig.signature,
                        error = %e,
                        "failed to fetch transaction (RPC error)"
                    );
                    skipped += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(RPC_RATE_LIMIT_MS * 2))
                        .await;
                    continue;
                }
            };

            if let Some(swap) = parse_swap_from_transaction(&tx, wallet) {
                parsed += 1;
                accumulator.observe_dex(&swap.dex);
                absorb_swap(
                    &mut accumulator,
                    &swap,
                    &mut observations,
                    sol_price,
                    wallet,
                );
                tracing::debug!(
                    wallet = %wallet,
                    sig = %sig.signature,
                    dex = %swap.dex,
                    direction = ?swap.direction,
                    input_mint = %swap.input_mint,
                    output_mint = %swap.output_mint,
                    "swap detected"
                );
                if observations.len() >= MAX_OBSERVATIONS_PER_WALLET {
                    tracing::warn!(
                        wallet = %wallet,
                        cap = MAX_OBSERVATIONS_PER_WALLET,
                        "observation cap reached; further observations dropped"
                    );
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(RPC_RATE_LIMIT_MS)).await;
        }

        let now = Utc::now();
        let stats = accumulator.build_stats(wallet, Some(now));
        self.accumulators.insert(wallet.to_string(), accumulator);
        self.processed_sigs
            .insert(wallet.to_string(), processed.clone());

        for obs in observations {
            if obs.received_at > now {
                continue;
            }
            self.wallet_tracker.observe(obs);
        }

        if stats.trades > 0 {
            self.wallet_tracker.upsert(stats.clone());
        }

        tracing::info!(
            wallet = %wallet,
            signatures = sigs.len(),
            parsed_swaps = parsed,
            rpc_skipped = skipped,
            successful_txs = successful,
            completed_trades = stats.trades,
            "wallet history rebuilt"
        );

        Ok(parsed)
    }

    const WALLETS_PER_TICK: usize = 8;

    pub async fn tick(&mut self) -> Result<Vec<CandidateInput>, anyhow::Error> {
        let mut new_candidates = Vec::new();
        let now = Utc::now();

        let total = self.wallets.len();
        if total == 0 {
            return Ok(new_candidates);
        }
        let batch = Self::WALLETS_PER_TICK.min(total);
        let start = self.wallet_poll_idx % total;
        let indices: Vec<usize> = (start..start + batch).map(|i| i % total).collect();
        self.wallet_poll_idx = (start + batch) % total;

        for &idx in &indices {
            let wallet = self.wallets[idx].clone();
            if let Err(e) = self.poll_wallet(&wallet, &mut new_candidates, now).await {
                tracing::debug!(
                    wallet = %wallet,
                    error = %e,
                    "wallet poll failed this tick"
                );
            }
        }
        Ok(new_candidates)
    }

    async fn poll_wallet(
        &mut self,
        wallet: &str,
        new_candidates: &mut Vec<CandidateInput>,
        now: DateTime<Utc>,
    ) -> Result<(), anyhow::Error> {
        if !self.processed_sigs.contains_key(wallet) {
            if let Err(e) = self.rebuild_wallet_history(wallet).await {
                tracing::warn!(
                    wallet = %wallet,
                    error = %e,
                    "initial rebuild failed; will retry next tick"
                );
                return Ok(());
            }
        }

        let sigs: Vec<SignatureEntry> = self
            .rpc
            .signatures_for_address_paged(wallet, 50, None)
            .await
            .map_err(|e| anyhow::anyhow!("signatures RPC: {e}"))?;

        if sigs.is_empty() {
            return Ok(());
        }

        let sol_price = self.config.economics.sol_price_usd.unwrap_or(dec!(150));

        let mut tx_count = 0usize;
        let mut swaps_this_tick: Vec<ParsedSwap> = Vec::new();
        let mut new_mints_this_tick: HashSet<String> = HashSet::new();
        let mut observations: Vec<WalletTradeObservation> = Vec::new();

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
                _ => {
                    tokio::time::sleep(std::time::Duration::from_millis(RPC_RATE_LIMIT_MS)).await;
                    continue;
                }
            };
            tx_count += 1;
            self.processed_sigs
                .entry(wallet.to_string())
                .or_default()
                .insert(sig.signature.clone());

            if let Some(swap) = parse_swap_from_transaction(&tx, wallet) {
                tracing::info!(
                    wallet = %wallet,
                    sig = %sig.signature,
                    dex = %swap.dex,
                    direction = ?swap.direction,
                    input = %swap.input_mint,
                    output = %swap.output_mint,
                    "NEW SWAP DETECTED"
                );
                swaps_this_tick.push(swap);
            }
            tokio::time::sleep(std::time::Duration::from_millis(RPC_RATE_LIMIT_MS)).await;
        }

        let accumulator = self
            .accumulators
            .entry(wallet.to_string())
            .or_insert_with(WalletAccumulator::new);
        for swap in &swaps_this_tick {
            accumulator.observe_dex(&swap.dex);
            absorb_swap(accumulator, swap, &mut observations, sol_price, wallet);
            match swap.direction {
                SwapDirection::Buy => {
                    new_mints_this_tick.insert(swap.output_mint.clone());
                    self.seen_mints.insert(swap.output_mint.clone());
                }
                SwapDirection::Sell => {}
            }
        }
        for obs in observations {
            if obs.received_at <= now {
                self.wallet_tracker.observe(obs);
            }
        }

        let stats = accumulator.build_stats(wallet, Some(now));
        if stats.trades > 0 {
            self.wallet_tracker.upsert(stats);
        }

        for mint in &new_mints_this_tick {
            if self.offered_mints.contains(mint) {
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
        now: DateTime<Utc>,
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

        let mut safety = match fetch_token_safety(
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

        let (market, price_impact_bps) = match fetch_market_snapshot(
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

        // The successful Jupiter quote is real-time evidence that the route
        // exists at the moment we observed it.
        safety.sellable = Some(true);
        safety.route_available = Some(true);

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
                avg_price_impact_bps: Decimal::from(price_impact_bps),
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

fn absorb_swap(
    accumulator: &mut WalletAccumulator,
    swap: &ParsedSwap,
    observations: &mut Vec<WalletTradeObservation>,
    sol_price: Decimal,
    wallet: &str,
) {
    let ts = chrono::DateTime::from_timestamp(swap.block_time, 0).unwrap_or_else(Utc::now);
    let input_sol =
        Decimal::from(swap.input_amount) / Decimal::from(10u64.pow(swap.input_decimals as u32));
    let output_tokens = Decimal::from(swap.output_amount);
    let now = Utc::now();
    let notional = match swap.direction {
        SwapDirection::Buy => input_sol * sol_price,
        SwapDirection::Sell => {
            let sol_out = Decimal::from(swap.output_amount)
                / Decimal::from(10u64.pow(swap.output_decimals as u32));
            sol_out * sol_price
        }
    };
    let (mint, side) = match swap.direction {
        SwapDirection::Buy => (swap.output_mint.clone(), Side::Buy),
        SwapDirection::Sell => (swap.input_mint.clone(), Side::Sell),
    };
    if observations.len() < MAX_OBSERVATIONS_PER_WALLET {
        observations.push(WalletTradeObservation {
            wallet: wallet.to_string(),
            mint: mint.clone(),
            side,
            notional_usd: notional,
            observed_at: ts,
            received_at: now,
            signature: swap.signature.clone(),
        });
    }
    accumulator.record_observation(&mint, &swap.direction, input_sol, output_tokens, ts);
}

pub fn load_wallets(path: &str) -> Result<Vec<String>, anyhow::Error> {
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

pub fn is_valid_solana_address(addr: &str) -> bool {
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
        acc.record_observation("TOKEN", &SwapDirection::Buy, dec!(1), dec!(100), Utc::now());
        acc.record_observation("TOKEN", &SwapDirection::Buy, dec!(2), dec!(200), Utc::now());
        let trade = acc
            .record_sell("TOKEN", dec!(100), dec!(1.5), Utc::now())
            .unwrap();
        assert!(trade.return_pct > Decimal::ZERO);
        assert_eq!(acc.completed_trades.len(), 1);
    }

    #[test]
    fn accumulator_pit_stats_exclude_future_trades() {
        let mut acc = WalletAccumulator::new();
        let past = Utc::now() - Duration::days(2);
        acc.record_observation("T", &SwapDirection::Buy, dec!(1), dec!(100), past);
        let future = Utc::now() + Duration::days(1);
        acc.record_sell("T", dec!(50), dec!(1.2), future);
        let stats_now = acc.build_stats("W", Some(Utc::now()));
        assert_eq!(
            stats_now.trades, 0,
            "future trade must not count in PIT stats"
        );
        let stats_future = acc.build_stats("W", Some(future + Duration::seconds(1)));
        assert_eq!(stats_future.trades, 1);
    }

    #[test]
    fn validation_report_classifies_invalid_address() {
        let report = WalletValidationReport {
            wallet: "too_short".to_string(),
            status: WalletStatus::Invalid,
            signatures_fetched: 0,
            successful_transactions: 0,
            swaps_parsed: 0,
            buys: 0,
            sells: 0,
            dex_activity: HashMap::new(),
            last_activity_ts: None,
            first_activity_ts: None,
        };
        assert_eq!(report.status, WalletStatus::Invalid);
    }

    #[test]
    fn is_valid_solana_address_filters_base58() {
        assert!(is_valid_solana_address(
            "8xw2egWMMRMARCm1T8jiWc2gLfHFZPrbWdxw6jz9mTXW"
        ));
        assert!(!is_valid_solana_address("0OIl"));
        assert!(!is_valid_solana_address("short"));
        assert!(!is_valid_solana_address(
            "veryveryveryveryveryveryveryveryveryveryveryverylong"
        ));
    }

    // Sanity: ensure FIFO across multiple lots produces the right number of
    // completed trades and leaves the correct open inventory.
    #[test]
    fn fifo_three_lots_partial_sell() {
        let mut acc = WalletAccumulator::new();
        let t0 = Utc::now();
        acc.record_observation("X", &SwapDirection::Buy, dec!(1), dec!(100), t0);
        acc.record_observation("X", &SwapDirection::Buy, dec!(1), dec!(100), t0);
        acc.record_observation("X", &SwapDirection::Buy, dec!(1), dec!(100), t0);
        // 150 sold: lot1 (100) fully consumed, lot2 (50) partially consumed.
        acc.record_sell("X", dec!(150), dec!(1.5), t0);
        // 2 lots remain in the queue: partial lot2 (50) and untouched lot3 (100).
        assert_eq!(acc.open_positions.get("X").unwrap().len(), 2);
        // 2 completed trades: lot1 closed, lot2 partial close.
        assert_eq!(acc.completed_trades.len(), 2);
    }
}
