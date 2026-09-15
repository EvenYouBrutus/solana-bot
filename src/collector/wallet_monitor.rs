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
/// Public mainnet nodes throttle `getTransaction` aggressively; the previous
/// 120 ms pace caused heavy 429 losses during history reconstruction, which
/// silently shrank wallet sample sizes.
const RPC_RATE_LIMIT_MS: u64 = 350;

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
        input_amount: Decimal,
        output_amount: Decimal,
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
                // Buy: input leg is SOL spent, output leg is tokens received.
                self.open_positions
                    .entry(mint.to_string())
                    .or_default()
                    .push_back(OpenPosition {
                        sol_spent: input_amount,
                        tokens_received: output_amount,
                        timestamp: ts,
                    });
            }
            SwapDirection::Sell => {
                self.sells += 1;
                // Sell: input leg is tokens sold, output leg is SOL received.
                self.record_sell(mint, input_amount, output_amount, ts);
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
        // FIFO across lots. Each consumed slice produces one completed-trade
        // record whose cost basis and proceeds cover ONLY the quantity
        // actually sold; a partially consumed lot carries its remaining cost
        // forward so later sells account correctly.
        let queue = self.open_positions.get_mut(mint)?;
        if queue.is_empty() || tokens_sold <= Decimal::ZERO {
            return None;
        }
        let proceeds_per_token = sol_received / tokens_sold;
        let mut remaining_to_sell = tokens_sold;
        let mut last_trade: Option<CompletedTrade> = None;
        while remaining_to_sell > Decimal::ZERO {
            let Some(front) = queue.front_mut() else {
                break;
            };
            if front.tokens_received <= Decimal::ZERO {
                queue.pop_front();
                continue;
            }
            let sold_from_lot = front.tokens_received.min(remaining_to_sell);
            let cost_of_sold = front.sol_spent * sold_from_lot / front.tokens_received;
            let proceeds = proceeds_per_token * sold_from_lot;
            let return_pct = if cost_of_sold > Decimal::ZERO {
                ((proceeds - cost_of_sold) / cost_of_sold * dec!(100)).round_dp(2)
            } else {
                Decimal::ZERO
            };
            last_trade = Some(CompletedTrade {
                return_pct,
                pnl_sol: proceeds - cost_of_sold,
                entry_time: front.timestamp,
                exit_time: sell_time,
            });
            front.tokens_received -= sold_from_lot;
            front.sol_spent -= cost_of_sold;
            let lot_exhausted = front.tokens_received.is_zero();
            if let Some(ref trade) = last_trade {
                self.completed_trades.push(trade.clone());
            }
            remaining_to_sell -= sold_from_lot;
            if lot_exhausted {
                queue.pop_front();
            }
        }
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
    /// Wallets whose history was already rebuilt (either by startup validation
    /// or lazily on the first poll). Prevents a second full RPC pass.
    rebuilt: HashSet<String>,
    /// Upper bound on RPC cost of the initial per-wallet reconstruction.
    history_scan_budget: usize,
    /// Set once the initial cohort sweep is complete; afterwards only
    /// genuinely new on-chain signatures produce candidates.
    initial_rebuild_done: bool,
    /// Guards the one-shot cohort validation summary log line.
    validation_summary_logged: bool,
    /// Candidates produced during the startup reconstruction (drained by the
    /// next tick). Keeps `validate_all`'s signature unchanged.
    pending_candidates: Vec<CandidateInput>,
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
        let scan_budget = config
            .wallet_monitor
            .max_history_signatures
            .clamp(1, DEFAULT_REBUILD_PAGE * MAX_REBUILD_PAGES) as usize;

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
            rebuilt: HashSet::new(),
            history_scan_budget: scan_budget,
            initial_rebuild_done: false,
            validation_summary_logged: false,
            pending_candidates: Vec::new(),
        };

        tracing::info!(
            "wallet monitor initialised; history reconstruction is bounded and runs at startup"
        );
        Ok(monitor)
    }

    pub fn validation_summary(&self) -> &ValidationSummary {
        &self.validation_summary
    }

    /// One-shot cohort validation (used by tests and tooling): rebuild every
    /// wallet exactly once and return the aggregated summary. The live
    /// session performs the same work incrementally, one wallet per tick, to
    /// stay responsive; both paths share `rebuild_wallet_history` and the
    /// report accumulator.
    pub async fn validate_all(&mut self) -> Result<ValidationSummary, anyhow::Error> {
        tracing::info!(
            wallets = self.wallets.len(),
            "starting wallet cohort validation"
        );
        for wallet in self.wallets.clone() {
            if self.rebuilt.contains(&wallet) {
                continue;
            }
            let report = self.rebuild_wallet_history(&wallet).await;
            self.record_validation_report(report);
        }
        self.initial_rebuild_done = true;
        self.maybe_log_validation_summary();
        Ok(self.validation_summary.clone())
    }

    /// Fold one wallet's validation report into the running summary.
    fn record_validation_report(&mut self, report: WalletValidationReport) {
        let summary = &mut self.validation_summary;
        summary.wallets_loaded = self.wallets.len() as u32;
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
            WalletStatus::Invalid | WalletStatus::Suspect => summary.wallets_invalid += 1,
        }
        summary.reports.push(report);
    }

    /// Once every configured wallet has been reconstructed, log the cohort
    /// composition exactly once so the operator can see the cohort state.
    fn maybe_log_validation_summary(&mut self) {
        if self.validation_summary_logged || self.rebuilt.len() < self.wallets.len() {
            return;
        }
        self.validation_summary_logged = true;
        let summary = self.validation_summary.clone();
        tracing::info!(
            wallets_loaded = summary.wallets_loaded,
            wallets_valid = summary.wallets_valid,
            wallets_active = summary.wallets_active,
            wallets_no_swap_activity = summary.wallets_no_swap_activity,
            wallets_low_activity = summary.wallets_low_activity,
            wallets_invalid = summary.wallets_invalid,
            signatures = summary.total_signatures,
            swaps = summary.total_swaps_parsed,
            "wallet cohort validation complete"
        );
        if summary.wallets_active == 0 {
            tracing::error!(
                wallets_loaded = summary.wallets_loaded,
                "no VALID_ACTIVE wallets in cohort; live paper experiment will not produce signals"
            );
        }
    }

    /// Rebuild a single wallet's recent history from RPC, seed the tracker and
    /// derive a validation report. This is the ONLY full-history pass per
    /// wallet: `poll_wallet` continues incrementally from the recorded
    /// signature set.
    async fn rebuild_wallet_history(&mut self, wallet: &str) -> WalletValidationReport {
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
        let pages = max_pages.clamp(1, MAX_REBUILD_PAGES);
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

        // Trim to the per-wallet scan budget so reconstruction cost stays
        // bounded even when the RPC returns large pages.
        all_sigs.truncate(self.history_scan_budget);

        let mut successful: u32 = 0;
        let mut tx_fetch_failed: u32 = 0;
        let mut tx_fetched: u32 = 0;
        let mut buys: u32 = 0;
        let mut sells: u32 = 0;
        let mut dex_activity: HashMap<String, u32> = HashMap::new();
        let mut swaps_parsed: u32 = 0;
        let mut last_ts: Option<i64> = None;
        let mut first_ts: Option<i64> = None;
        let mut processed = HashSet::new();
        let mut accumulator = WalletAccumulator::new();
        let sol_price = self.config.economics.sol_price_usd.unwrap_or(dec!(150));
        let mut observations: Vec<WalletTradeObservation> = Vec::new();
        let consensus_window = self.consensus_window_secs as i64;
        let now_for_window = Utc::now();
        // Mints bought by this wallet within the consensus window. Only these
        // are eligible for candidate construction after reconstruction; older
        // historical trades are context for wallet scoring, never signals.
        let mut recent_buy_mints: HashSet<String> = HashSet::new();

        for sig in &all_sigs {
            if sig.err.is_some() {
                continue;
            }
            successful += 1;
            if let Some(bt) = sig.block_time {
                last_ts = Some(match last_ts {
                    Some(prev) => prev.max(bt),
                    None => bt,
                });
                first_ts = Some(match first_ts {
                    Some(prev) => prev.min(bt),
                    None => bt,
                });
            }
            let tx = match self.rpc.transaction(&sig.signature).await {
                Ok(Some(t)) => {
                    tx_fetched += 1;
                    t
                }
                Ok(None) => {
                    tokio::time::sleep(std::time::Duration::from_millis(RPC_RATE_LIMIT_MS)).await;
                    continue;
                }
                Err(e) => {
                    // Explicitly skipped: a failed fetch is never counted as
                    // a successful or failed swap, so wallet statistics are
                    // not corrupted. The signature is not marked processed
                    // and is retried on a later poll.
                    tx_fetch_failed += 1;
                    tracing::warn!(
                        wallet = %wallet,
                        sig = %sig.signature,
                        error = %e,
                        fetch_failed_total = tx_fetch_failed,
                        "tx fetch failed during history reconstruction; signature left unprocessed for retry"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(RPC_RATE_LIMIT_MS * 2))
                        .await;
                    continue;
                }
            };
            // Mark processed only after a successful fetch so rate-limited
            // signatures are retried on later polls instead of being lost.
            processed.insert(sig.signature.clone());
            if let Some(swap) = parse_swap_from_transaction(&tx, wallet) {
                swaps_parsed += 1;
                accumulator.observe_dex(&swap.dex);
                dex_activity
                    .entry(swap.dex.clone())
                    .and_modify(|c| *c += 1)
                    .or_insert(1);
                match swap.direction {
                    SwapDirection::Buy => {
                        buys += 1;
                        // Genuinely recent only: within the consensus window
                        // of now, and never in the future.
                        let age_secs = now_for_window.timestamp() - swap.block_time;
                        let in_window = swap.block_time <= now_for_window.timestamp()
                            && age_secs <= consensus_window;
                        tracing::debug!(
                            wallet = %wallet,
                            sig = %swap.signature,
                            mint = %swap.output_mint,
                            block_time = swap.block_time,
                            age_secs = age_secs,
                            recent_window_secs = consensus_window,
                            inside_recent_window = in_window,
                            rejection_reason = if in_window { "" } else { "buy is older than the consensus recent-buy window" },
                            "historical BUY considered"
                        );
                        if in_window {
                            recent_buy_mints.insert(swap.output_mint.clone());
                        }
                    }
                    SwapDirection::Sell => sells += 1,
                }
                absorb_swap(
                    &mut accumulator,
                    &swap,
                    &mut observations,
                    sol_price,
                    wallet,
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(RPC_RATE_LIMIT_MS)).await;
        }

        let now = Utc::now();
        let stats = accumulator.build_stats(wallet, Some(now));
        self.accumulators.insert(wallet.to_string(), accumulator);
        self.processed_sigs.insert(wallet.to_string(), processed);
        self.rebuilt.insert(wallet.to_string());

        for obs in observations {
            if obs.received_at > now {
                continue;
            }
            self.wallet_tracker.observe(obs);
        }
        if stats.trades > 0 {
            self.wallet_tracker.upsert(stats.clone());
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
            tx_fetched = tx_fetched,
            tx_fetch_failed = tx_fetch_failed,
            swaps_parsed = swaps_parsed,
            buys = buys,
            sells = sells,
            completed_trades = stats.trades,
            recent_buy_mints = recent_buy_mints.len(),
            "wallet history reconstruction summary"
        );

        // Candidate construction for genuinely recent BUYs found in history.
        // Consensus is still required downstream: a single historical trade
        // can never produce a trade on its own. A mint that fails consensus
        // here is NOT marked seen, so a later wallet's rebuild (or a live
        // swap) can retry it once real consensus exists.
        for mint in &recent_buy_mints {
            if self.offered_mints.contains(mint) {
                tracing::debug!(
                    mint = %mint,
                    added_to_pending = false,
                    rejection_reason = "mint already offered as a candidate",
                    "recent BUY candidate gate"
                );
                continue;
            }
            tracing::debug!(
                mint = %mint,
                added_to_pending = true,
                rejection_reason = "",
                "recent BUY candidate gate"
            );
            let mut candidates = Vec::new();
            self.check_and_build_candidate(mint, &mut candidates, now)
                .await;
            self.pending_candidates.extend(candidates);
        }

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

    const WALLETS_PER_TICK: usize = 8;

    pub async fn tick(&mut self) -> Result<Vec<CandidateInput>, anyhow::Error> {
        let mut new_candidates = Vec::new();
        let now = Utc::now();

        let total = self.wallets.len();
        if total == 0 {
            return Ok(new_candidates);
        }
        // Candidates produced during the startup reconstruction are released
        // on the first tick; the pipeline then continues with live polling.
        if !self.pending_candidates.is_empty() {
            new_candidates = std::mem::take(&mut self.pending_candidates);
            tracing::info!(
                count = new_candidates.len(),
                "releasing candidates reconstructed from recent wallet history"
            );
            return Ok(new_candidates);
        }
        // Complete the startup sweep incrementally: one wallet per tick keeps
        // the session responsive (exit monitor, interlocks, and candidate
        // draining all run between rebuilds) instead of blocking startup for
        // minutes on RPC latency. Candidates produced by each rebuild drain
        // on the following tick.
        if !self.initial_rebuild_done {
            let next = self
                .wallets
                .iter()
                .find(|w| !self.rebuilt.contains(*w))
                .cloned();
            match next {
                Some(wallet) => {
                    let report = self.rebuild_wallet_history(&wallet).await;
                    self.record_validation_report(report);
                    self.maybe_log_validation_summary();
                    return Ok(new_candidates);
                }
                None => self.initial_rebuild_done = true,
            }
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
        if !self.rebuilt.contains(wallet) {
            // The startup sweep rebuilds one wallet per tick to keep the
            // session responsive; live polling skips it until then.
            return Ok(());
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
                Ok(None) => {
                    tokio::time::sleep(std::time::Duration::from_millis(RPC_RATE_LIMIT_MS)).await;
                    continue;
                }
                Err(e) => {
                    // Explicit skip: the fetch failed, so this transaction is
                    // neither a successful nor a failed swap. The signature
                    // stays unprocessed and is retried on a later poll.
                    tracing::warn!(
                        wallet = %wallet,
                        sig = %sig.signature,
                        error = %e,
                        "tx fetch failed during live poll; left unprocessed for retry"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(RPC_RATE_LIMIT_MS * 2))
                        .await;
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
                    "new swap detected"
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

        // Canonical mint decimals are required for atomic/USD conversion.
        // Fail closed when the chain cannot confirm them: a wrong guess
        // silently corrupts every downstream price and quantity.
        let token_decimals = match self.rpc.mint_account_info(mint).await {
            Ok(Some(info)) if info.is_initialized => info.decimals,
            _ => {
                tracing::info!(mint = %mint, "mint decimals unverifiable; candidate rejected");
                return;
            }
        };

        let sol_price = self.config.economics.sol_price_usd.unwrap_or(dec!(150));
        let base_mint_decimals = 9u8;
        let input_amount = (self.position_usd / sol_price * dec!(1_000_000_000))
            .to_string()
            .parse::<u64>()
            .unwrap_or(4_000_000);

        let (market, price_impact_bps) = match fetch_market_snapshot(
            self.executor.as_ref(),
            mint,
            sol_price,
            base_mint_decimals,
            token_decimals,
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

        let avg_return: Decimal = if consensus_wallets.is_empty() {
            Decimal::ZERO
        } else {
            consensus_wallets
                .iter()
                .map(|w| w.avg_return_pct)
                .sum::<Decimal>()
                / Decimal::from(consensus_wallets.len())
        };
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
            wallets = %candidate.wallets.iter().map(|w| w.wallet.as_str()).collect::<Vec<_>>().join(","),
            "recent BUY candidate created"
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
    // Both legs normalized to human units (atomic / 10^decimals). For a Buy
    // the input leg is SOL and the output leg is the token; for a Sell the
    // input leg is the token and the output leg is SOL.
    let input_leg =
        Decimal::from(swap.input_amount) / Decimal::from(10u64.pow(swap.input_decimals as u32));
    let output_leg =
        Decimal::from(swap.output_amount) / Decimal::from(10u64.pow(swap.output_decimals as u32));
    let now = Utc::now();
    let notional = match swap.direction {
        SwapDirection::Buy => input_leg * sol_price,
        SwapDirection::Sell => output_leg * sol_price,
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
    accumulator.record_observation(&mint, &swap.direction, input_leg, output_leg, ts);
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

    // Regression: absorb_swap must normalize atomic amounts to human units
    // (divide by the leg's decimals) before FIFO accounting. Previously the
    // token leg was left in raw atomic units while the SOL leg was human,
    // producing absurd return percentages that made real wallets unable to
    // reach the qualified score.
    #[test]
    fn absorb_swap_normalizes_decimals_for_fifo() {
        let mut acc = WalletAccumulator::new();
        let mut obs = Vec::new();
        // BUY: 1 SOL (9dp) -> 1.0 token (6dp = 1_000_000 atomic).
        let buy = ParsedSwap {
            wallet: "w".into(),
            input_mint: "So11111111111111111111111111111111111111112".into(),
            output_mint: "T".into(),
            input_amount: 1_000_000_000,
            output_amount: 1_000_000,
            input_decimals: 9,
            output_decimals: 6,
            direction: SwapDirection::Buy,
            fee_lamports: 5000,
            dex: "jupiter_v6".into(),
            slot: 1,
            block_time: 1_700_000_000,
            signature: "s1".into(),
        };
        absorb_swap(&mut acc, &buy, &mut obs, dec!(150), "w");
        // SELL: 1.0 token (6dp) -> 2 SOL.
        let sell = ParsedSwap {
            input_amount: 1_000_000,
            input_decimals: 6,
            input_mint: "T".into(),
            output_mint: "So11111111111111111111111111111111111111112".into(),
            output_amount: 2_000_000_000,
            output_decimals: 9,
            direction: SwapDirection::Sell,
            signature: "s2".into(),
            ..buy.clone()
        };
        absorb_swap(&mut acc, &sell, &mut obs, dec!(150), "w");
        // Lot consumed fully, one completed trade, +100% return.
        assert_eq!(acc.completed_trades.len(), 1);
        let trade = &acc.completed_trades[0];
        assert_eq!(
            trade.return_pct,
            dec!(100.00),
            "sell of a 1-SOL lot for 2 SOL must be +100%, got {}",
            trade.return_pct
        );
        assert_eq!(trade.pnl_sol, dec!(1));
        // Observation notionals are in USD: buy = 1 SOL * 150.
        assert_eq!(obs[0].notional_usd, dec!(150));
    }

    // --- Partial-sell accounting regressions ---

    fn open_lot(
        acc: &mut WalletAccumulator,
        mint: &str,
        sol_spent: Decimal,
        tokens: Decimal,
        ts: DateTime<Utc>,
    ) {
        acc.record_observation(mint, &SwapDirection::Buy, sol_spent, tokens, ts);
    }

    // Regression: a partial sell must book cost basis only for the sold
    // quantity. Previously the whole lot's cost was subtracted, producing
    // absurd (hugely negative) PnL and corrupting win rates.
    #[test]
    fn partial_sell_books_proportional_cost_not_whole_lot() {
        let mut acc = WalletAccumulator::new();
        let t0 = Utc::now();
        // Buy 100 tokens for 1 SOL; sell 10 for 0.2 SOL (price doubled).
        open_lot(&mut acc, "T", dec!(1), dec!(100), t0);
        let trade = acc
            .record_sell("T", dec!(10), dec!(0.2), t0)
            .expect("partial sell must realize a trade");
        // Cost of sold slice = 0.1 SOL; proceeds 0.2 SOL -> +100%.
        assert_eq!(trade.return_pct, dec!(100.00));
        assert_eq!(trade.pnl_sol, dec!(0.1));
        // Remaining inventory: 90 tokens with 0.9 SOL cost carried forward.
        let lot = &acc.open_positions.get("T").unwrap()[0];
        assert_eq!(lot.tokens_received, dec!(90));
        assert_eq!(lot.sol_spent, dec!(0.9));
    }

    #[test]
    fn full_sell_closes_lot_completely() {
        let mut acc = WalletAccumulator::new();
        let t0 = Utc::now();
        open_lot(&mut acc, "T", dec!(2), dec!(50), t0);
        let trade = acc
            .record_sell("T", dec!(50), dec!(3), t0)
            .expect("full sell must realize a trade");
        assert_eq!(trade.return_pct, dec!(50.00));
        assert_eq!(trade.pnl_sol, dec!(1));
        // Queue fully drained.
        assert!(acc.open_positions.get("T").unwrap().is_empty());
        assert_eq!(acc.completed_trades.len(), 1);
    }

    #[test]
    fn multiple_partial_sells_each_book_own_slice() {
        let mut acc = WalletAccumulator::new();
        let t0 = Utc::now();
        // Buy 100 tokens for 1 SOL. Sell 25 @ 0.5 SOL (price doubled),
        // then 25 more @ 0.1 SOL (price dropped below entry).
        open_lot(&mut acc, "T", dec!(1), dec!(100), t0);
        let t1 = acc
            .record_sell("T", dec!(25), dec!(0.5), t0)
            .expect("first partial sell");
        assert_eq!(t1.return_pct, dec!(100.00));
        let t2 = acc
            .record_sell("T", dec!(25), dec!(0.1), t0)
            .expect("second partial sell");
        // Cost basis 0.25 SOL, proceeds 0.1 SOL -> -60%.
        assert_eq!(t2.return_pct, dec!(-60.00));
        assert_eq!(t2.pnl_sol, dec!(-0.15));
        // Remaining: 50 tokens at 0.5 SOL cost.
        let lot = &acc.open_positions.get("T").unwrap()[0];
        assert_eq!(lot.tokens_received, dec!(50));
        assert_eq!(lot.sol_spent, dec!(0.5));
        assert_eq!(acc.completed_trades.len(), 2);
        // Aggregate stats: 1 win, 1 loss; PnL = (0.5-0.25) + (0.1-0.25) = +0.10.
        let stats = acc.build_stats("W", Some(t0 + Duration::seconds(1)));
        assert_eq!(stats.trades, 2);
        assert_eq!(stats.win_rate, dec!(0.5));
        assert_eq!(stats.realized_pnl_usd, dec!(0.10));
    }

    #[test]
    fn sell_spanning_two_lots_books_each_lot_correctly() {
        let mut acc = WalletAccumulator::new();
        let t0 = Utc::now();
        // Lot1: 100 tokens @ 1 SOL; lot2: 100 tokens @ 3 SOL.
        open_lot(&mut acc, "T", dec!(1), dec!(100), t0);
        open_lot(&mut acc, "T", dec!(3), dec!(100), t0);
        // Sell 150 for 2 SOL -> 100 from lot1 (cost 1.0) + 50 from lot2 (cost 1.5).
        let trade = acc
            .record_sell("T", dec!(150), dec!(2), t0)
            .expect("spanning sell");
        // Last consumed slice: 50 from lot2 at cost 1.5, proceeds 2*50/150
        // = 0.666... -> return = (0.6667-1.5)/1.5 = -55.56%.
        assert_eq!(trade.return_pct.round_dp(2), dec!(-55.56));
        // Remaining: 50 from lot2 at 1.5 SOL cost.
        let remaining = &acc.open_positions.get("T").unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].tokens_received, dec!(50));
        assert_eq!(remaining[0].sol_spent, dec!(1.5));
        assert_eq!(acc.completed_trades.len(), 2);
    }

    #[test]
    fn oversell_stops_at_available_inventory() {
        let mut acc = WalletAccumulator::new();
        let t0 = Utc::now();
        open_lot(&mut acc, "T", dec!(1), dec!(100), t0);
        // Sell more than held: only the available 100 are accounted.
        let trade = acc.record_sell("T", dec!(150), dec!(2), t0);
        assert!(trade.is_some());
        assert!(acc.open_positions.get("T").unwrap().is_empty());
        assert_eq!(acc.completed_trades.len(), 1);
    }
}
