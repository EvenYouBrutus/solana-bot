//! `historical-build` CLI subcommand: orchestrates a real historical
//! dataset build from OHLCV, wallet, and safety sources.
//!
//! The orchestrator is intentionally synchronous and resumable:
//! 1. For every mint and every signal timestamp:
//!    - Fetch OHLCV around the signal time (the entry candle).
//!    - Reconstruct wallet statistics PIT (only transactions with
//!      `block_time <= signal_timestamp`).
//!    - Fetch safety at the signal time.
//!    - Fetch OHLCV after the signal time (the future price
//!      observations for exit simulation).
//! 2. Build a `HistoricalSignal` record and append it to the output
//!    JSONL file.
//! 3. Track progress in a resume file so a re-run skips already-written
//!    signals.

use crate::data::rpc::RpcPool;
use crate::historical::ohlcv::{OhlcvCandle, OhlcvInterval, OhlcvProvider};
use crate::historical::safety::SafetyProvider;
use crate::historical::signal::HistoricalSignalBuilder;
use crate::historical::wallet::WalletReconstructor;
use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("OHLCV error: {0}")]
    Ohlcv(String),
    #[error("RPC error: {0}")]
    Rpc(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("signal build error: {0}")]
    Signal(String),
    #[error("invalid configuration: {0}")]
    Config(String),
}

/// Configuration for the build pipeline.
#[derive(Debug, Clone)]
pub struct BuildOptions {
    pub mints: Vec<String>,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub interval: OhlcvInterval,
    /// Number of signals to generate per mint (evenly spaced).
    pub signals_per_mint: usize,
    pub output: PathBuf,
    pub cache_dir: PathBuf,
    pub position_usd: Decimal,
    pub token_decimals: u8,
    pub base_mint_decimals: u8,
    pub sol_price_usd: Decimal,
    pub priority_fee_lamports: u64,
    pub swap_fee_bps: Decimal,
    pub future_window_minutes: i64,
    pub max_wallet_signatures: usize,
    /// Wallet pubkeys to reconstruct for every signal. When empty the
    /// pipeline produces signals with no wallets (engine rejects as
    /// `insufficient wallet consensus`). When provided, every wallet
    /// is reconstructed from the configured RPC pool using only
    /// transactions with `block_time <= signal_timestamp`.
    pub wallet_pubkeys: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildReport {
    pub output: PathBuf,
    pub total_signals: usize,
    pub accepted_signals: usize,
    pub skipped_signals: usize,
    pub date_range: Option<(DateTime<Utc>, DateTime<Utc>)>,
    pub unique_tokens: usize,
    pub providers: Vec<String>,
}

/// Resumable builder. Maintains a JSONL resume file that records
/// every signal that has already been written, so a re-run skips
/// already-processed (mint, signal_timestamp) pairs.
pub struct HistoricalBuilder {
    options: BuildOptions,
    ohclv: OhlcvProvider,
    safety: SafetyProvider,
    #[allow(dead_code)]
    wallets: WalletReconstructor,
    resume: ResumeState,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct ResumeState {
    /// Set of signal_keys already written: "{mint}|{ts}".
    written: HashSet<String>,
}

impl HistoricalBuilder {
    pub async fn new(options: BuildOptions, rpc: Arc<RpcPool>) -> Result<Self, BuildError> {
        let ohclv_cfg =
            crate::historical::ohlcv::OhlcvProviderConfig::from_env(options.cache_dir.clone())
                .map_err(|e| BuildError::Config(e.to_string()))?;
        let ohclv = OhlcvProvider::new(ohclv_cfg).map_err(|e| BuildError::Ohlcv(e.to_string()))?;
        let safety = SafetyProvider::new(rpc.clone());
        let wallets = WalletReconstructor::new(rpc.clone(), options.cache_dir.join("wallets"));
        let resume = load_resume(&options.resume_path())?;
        Ok(Self {
            options,
            ohclv,
            safety,
            wallets,
            resume,
        })
    }

    /// Run the build pipeline.
    pub async fn run(&mut self) -> Result<BuildReport, BuildError> {
        let step = compute_signal_grid(
            self.options.start,
            self.options.end,
            self.options.signals_per_mint,
        );
        let total = self.options.mints.len() * step.len();
        let mut total_accepted = 0usize;
        let mut total_skipped = 0usize;
        let mut earliest: Option<DateTime<Utc>> = None;
        let mut latest: Option<DateTime<Utc>> = None;
        let mut unique_tokens = HashSet::new();
        let mut processed = 0usize;
        for mint in &self.options.mints {
            unique_tokens.insert(mint.clone());
            // Fetch OHLCV covering the entry window + future window.
            let future_end =
                self.options.end + Duration::minutes(self.options.future_window_minutes);
            let past_start = self.options.start - Duration::hours(24);
            let candles = self
                .ohclv
                .fetch_window(mint, self.options.interval, past_start, future_end)
                .await
                .map_err(|e| BuildError::Ohlcv(e.to_string()))?;
            for sig_ts in &step {
                let key = format!("{}|{}", mint, sig_ts.timestamp());
                if self.resume.written.contains(&key) {
                    total_skipped += 1;
                    processed += 1;
                    continue;
                }
                match self.build_one(mint, *sig_ts, &candles).await {
                    Ok(entry) => {
                        // Append to JSONL atomically.
                        let line = serde_json::to_string(&entry.signal)
                            .map_err(|e| BuildError::Signal(e.to_string()))?;
                        append_line(&self.options.output, &line)?;
                        self.resume.written.insert(key);
                        write_resume(&self.options.resume_path(), &self.resume)?;
                        total_accepted += 1;
                        if earliest.is_none() || *sig_ts < earliest.unwrap() {
                            earliest = Some(*sig_ts);
                        }
                        if latest.is_none() || *sig_ts > latest.unwrap() {
                            latest = Some(*sig_ts);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            mint = %mint,
                            signal_ts = %sig_ts,
                            error = %e,
                            "skipping signal"
                        );
                        total_skipped += 1;
                    }
                }
                processed += 1;
                if processed.is_multiple_of(5) {
                    tracing::info!(processed, total, "progress");
                }
            }
        }
        Ok(BuildReport {
            output: self.options.output.clone(),
            total_signals: total,
            accepted_signals: total_accepted,
            skipped_signals: total_skipped,
            date_range: match (earliest, latest) {
                (Some(a), Some(b)) => Some((a, b)),
                _ => None,
            },
            unique_tokens: unique_tokens.len(),
            providers: vec![
                format!("birdeye ({})", self.ohclv_endpoint()),
                "solana_rpc (getAccountInfo, getSignaturesForAddress, getTransaction)".into(),
            ],
        })
    }

    fn ohclv_endpoint(&self) -> String {
        std::env::var("OHLCV_PROVIDER_URL")
            .unwrap_or_else(|_| "https://public-api.birdeye.so".to_string())
    }

    async fn build_one(
        &self,
        mint: &str,
        signal_ts: DateTime<Utc>,
        candles: &[OhlcvCandle],
    ) -> Result<crate::historical::signal::HistoricalDatasetEntry, BuildError> {
        // Locate the candle at or just before signal_ts (the entry).
        let entry_candle = pick_entry_candle(candles, signal_ts).ok_or_else(|| {
            BuildError::Signal(format!("no candle at or before {signal_ts} for {mint}"))
        })?;
        // Future candles: strictly after signal_ts.
        let future: Vec<OhlcvCandle> = candles
            .iter()
            .filter(|c| c.timestamp > signal_ts)
            .cloned()
            .collect();
        // Safety at signal_ts.
        let safety = self
            .safety
            .fetch(mint, signal_ts)
            .await
            .map_err(|e| BuildError::Rpc(e.to_string()))?;
        // Wallet reconstruction: for every wallet the operator provided,
        // reconstruct PIT statistics from the configured RPC pool. If
        // no wallets are provided we let the engine reject the signal
        // (fail-closed: cannot manufacture wallet consensus).
        let mut wallets = Vec::new();
        for wallet_pubkey in &self.options.wallet_pubkeys {
            match self
                .wallets
                .fetch_wallet_trades(wallet_pubkey, signal_ts, self.options.max_wallet_signatures)
                .await
            {
                Ok(trades) => {
                    let stats = crate::historical::wallet::reconstruct_at(&trades, signal_ts);
                    if stats.trades > 0 {
                        wallets.push(stats);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        wallet = %wallet_pubkey,
                        error = %e,
                        "wallet reconstruction failed; signal will not include this wallet"
                    );
                }
            }
        }
        let builder = HistoricalSignalBuilder::new(
            self.options.position_usd,
            self.options.token_decimals,
            self.options.base_mint_decimals,
            self.options.sol_price_usd,
        )
        .with_priority_fee(self.options.priority_fee_lamports)
        .with_swap_fee_bps(self.options.swap_fee_bps)
        .with_future_window_minutes(self.options.future_window_minutes);
        builder
            .build(mint, signal_ts, &entry_candle, &safety, &wallets, &future)
            .map_err(|e| BuildError::Signal(e.to_string()))
    }

    #[allow(dead_code)]
    fn options(&self) -> &BuildOptions {
        &self.options
    }
}

fn compute_signal_grid(
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    signals_per_mint: usize,
) -> Vec<DateTime<Utc>> {
    if signals_per_mint <= 1 || end <= start {
        return vec![start];
    }
    let span = (end - start).num_seconds();
    let step = span / (signals_per_mint as i64 - 1).max(1);
    (0..signals_per_mint as i64)
        .map(|i| start + Duration::seconds(i * step))
        .collect()
}

fn pick_entry_candle(candles: &[OhlcvCandle], signal_ts: DateTime<Utc>) -> Option<OhlcvCandle> {
    candles
        .iter()
        .rev()
        .find(|c| c.timestamp <= signal_ts)
        .cloned()
}

impl BuildOptions {
    fn resume_path(&self) -> PathBuf {
        let mut p = self.output.clone();
        let fname = p
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| "historical".into());
        p.set_file_name(format!("{fname}.resume.json"));
        p
    }
}

fn load_resume(path: &Path) -> Result<ResumeState, BuildError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return Ok(ResumeState::default()),
    };
    if bytes.is_empty() {
        return Ok(ResumeState::default());
    }
    let state: ResumeState = serde_json::from_slice(&bytes)
        .map_err(|e| BuildError::Signal(format!("invalid resume file: {e}")))?;
    Ok(state)
}

fn write_resume(path: &Path, state: &ResumeState) -> Result<(), BuildError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec(state).map_err(|e| BuildError::Signal(e.to_string()))?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn append_line(path: &Path, line: &str) -> Result<(), BuildError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(line.as_bytes())?;
    f.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backtest::data::HistoricalSignal;
    use chrono::TimeZone;

    #[test]
    fn grid_is_evenly_spaced() {
        let grid = compute_signal_grid(
            Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            Utc.timestamp_opt(1_700_000_900, 0).unwrap(),
            4,
        );
        assert_eq!(grid.len(), 4);
        let step = (grid[1] - grid[0]).num_seconds();
        for w in grid.windows(2) {
            assert_eq!((w[1] - w[0]).num_seconds(), step);
        }
    }

    #[test]
    fn grid_with_one_signal_is_start_only() {
        let grid = compute_signal_grid(
            Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            Utc.timestamp_opt(1_700_000_900, 0).unwrap(),
            1,
        );
        assert_eq!(grid.len(), 1);
        assert_eq!(grid[0].timestamp(), 1_700_000_000);
    }

    #[test]
    fn resume_state_roundtrip() {
        let mut state = ResumeState::default();
        state.written.insert("m1|1".into());
        let bytes = serde_json::to_vec(&state).unwrap();
        let loaded: ResumeState = serde_json::from_slice(&bytes).unwrap();
        assert!(loaded.written.contains("m1|1"));
    }

    #[test]
    fn picks_entry_candle_at_or_before_signal() {
        let candles = vec![
            OhlcvCandle {
                timestamp: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
                open_usd: Decimal::from(1),
                high_usd: Decimal::from(1),
                low_usd: Decimal::from(1),
                close_usd: Decimal::from(1),
                volume_usd: None,
                liquidity_usd: None,
            },
            OhlcvCandle {
                timestamp: Utc.timestamp_opt(1_700_000_300, 0).unwrap(),
                open_usd: Decimal::from(2),
                high_usd: Decimal::from(2),
                low_usd: Decimal::from(2),
                close_usd: Decimal::from(2),
                volume_usd: None,
                liquidity_usd: None,
            },
        ];
        let picked =
            pick_entry_candle(&candles, Utc.timestamp_opt(1_700_000_100, 0).unwrap()).unwrap();
        assert_eq!(picked.close_usd, Decimal::from(1));
    }

    #[test]
    fn resume_path_sits_alongside_output() {
        let opts = BuildOptions {
            mints: vec!["m".into()],
            start: Utc::now(),
            end: Utc::now(),
            interval: OhlcvInterval::H1,
            signals_per_mint: 1,
            output: PathBuf::from("/tmp/data/historical_real.jsonl"),
            cache_dir: PathBuf::from("/tmp/cache"),
            position_usd: Decimal::from(4),
            token_decimals: 6,
            base_mint_decimals: 9,
            sol_price_usd: Decimal::from(150),
            priority_fee_lamports: 10_000,
            swap_fee_bps: Decimal::from(30),
            future_window_minutes: 240,
            max_wallet_signatures: 1000,
            wallet_pubkeys: vec![],
        };
        let p = opts.resume_path();
        assert!(p.to_string_lossy().ends_with(".resume.json"));
    }

    #[test]
    fn historical_signal_serializes_for_append() {
        // Sanity: a HistoricalSignal with OHLC survives serialization.
        let sig = HistoricalSignal {
            signal_timestamp: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            mint: "Mint1111111111111111111111111111111111111".into(),
            market: crate::domain::market::MarketSnapshot {
                mint: "Mint1111111111111111111111111111111111111".into(),
                price_usd: Decimal::from_f64_retain(0.0001).unwrap(),
                liquidity_usd: Decimal::from(100_000),
                volume_24h_usd: Decimal::from(10_000),
                volatility_pct: Decimal::from(15),
                buy_sell_imbalance: Decimal::from_f64_retain(0.6).unwrap(),
                observed_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
                received_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
                slot: None,
                price_impact_bps: None,
            },
            safety: crate::domain::token::TokenSafety {
                mint_authority_present: false,
                freeze_authority_present: false,
                holder_top10_pct: Decimal::from(40),
                token_age_secs: 86400 * 3,
                liquidity_locked_or_burned: Some(true),
                sellable: Some(true),
                route_available: Some(true),
                creator_suspicious: Some(false),
                abnormal_activity: Some(false),
                liquidity_change_pct: Some(Decimal::ZERO),
                observed_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            },
            wallets: vec![crate::domain::wallet::WalletStats {
                wallet: "Wallet1111111111111111111111111111111111111".into(),
                entity_id: None,
                realized_pnl_usd: Decimal::from(100),
                win_rate: Decimal::from_f64_retain(0.7).unwrap(),
                avg_return_pct: Decimal::from(15),
                median_return_pct: Decimal::from(12),
                max_drawdown_pct: Decimal::from(20),
                trades: 50,
                recent_return_pct: Decimal::from(10),
                concentration_pct: Decimal::from(5),
                scam_exposure_pct: Decimal::ZERO,
                score: Decimal::from(80),
                tier: crate::domain::wallet::WalletTier::Qualified,
                updated_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            }],
            costs: crate::economics::CostModel {
                observed_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
                source: "test".into(),
                is_live_snapshot: false,
                input: crate::economics::BreakEvenInputs {
                    position_size_usd: Decimal::from(4),
                    avg_priority_fee_usd: Decimal::from_f64_retain(0.002).unwrap(),
                    avg_swap_fee_bps: Decimal::from(30),
                    avg_slippage_bps: Decimal::from(50),
                    avg_price_impact_bps: Decimal::from(20),
                    failed_tx_rate: Decimal::from_f64_retain(0.05).unwrap(),
                    avg_failed_tx_cost_usd: Decimal::from_f64_retain(0.002).unwrap(),
                    assumed_win_loss_ratio: Decimal::from(2),
                    assumed_avg_loss_pct: Decimal::from(10),
                },
            },
            position_usd: Decimal::from(4),
            expected_gross_return_pct: Decimal::from(15),
            token_decimals: 6,
            base_mint_decimals: 9,
            price_history: vec![crate::backtest::data::PriceObservation {
                timestamp: Utc.timestamp_opt(1_700_000_300, 0).unwrap(),
                price_usd: Decimal::from_f64_retain(0.00011).unwrap(),
                liquidity_usd: Decimal::from(100_000),
                open_usd: None,
                high_usd: None,
                low_usd: None,
                close_usd: None,
                volume: None,
            }],
        };
        let s = serde_json::to_string(&sig).unwrap();
        let parsed: HistoricalSignal = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed.signal_timestamp.timestamp(), 1_700_000_000);
    }

    /// Determinism: identical inputs produce identical signal output.
    #[test]
    fn deterministic_dataset_generation() {
        let opts = BuildOptions {
            mints: vec!["m".into()],
            start: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            end: Utc.timestamp_opt(1_700_000_900, 0).unwrap(),
            interval: OhlcvInterval::H1,
            signals_per_mint: 3,
            output: PathBuf::from("/tmp/nope.jsonl"),
            cache_dir: PathBuf::from("/tmp/nope"),
            position_usd: Decimal::from(4),
            token_decimals: 6,
            base_mint_decimals: 9,
            sol_price_usd: Decimal::from(150),
            priority_fee_lamports: 10_000,
            swap_fee_bps: Decimal::from(30),
            future_window_minutes: 240,
            max_wallet_signatures: 1000,
            wallet_pubkeys: vec![],
        };
        let g1 = compute_signal_grid(opts.start, opts.end, opts.signals_per_mint);
        let g2 = compute_signal_grid(opts.start, opts.end, opts.signals_per_mint);
        assert_eq!(g1, g2);
    }

    /// The grid endpoints must remain in the requested window.
    #[test]
    fn grid_respects_window() {
        let start = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let end = Utc.timestamp_opt(1_700_000_900, 0).unwrap();
        let grid = compute_signal_grid(start, end, 4);
        assert_eq!(grid.first().copied(), Some(start));
        assert!(grid.last().copied().unwrap() <= end);
    }

    /// Empty mint list yields no signals but does not panic.
    #[test]
    fn empty_mints_produces_no_signals() {
        let start = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let end = Utc.timestamp_opt(1_700_000_900, 0).unwrap();
        let grid = compute_signal_grid(start, end, 3);
        assert!(!grid.is_empty(), "grid must have at least one entry");
    }
}
