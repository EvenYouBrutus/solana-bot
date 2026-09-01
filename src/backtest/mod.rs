pub mod data;
pub mod engine;
pub mod split;
pub mod stats;

use crate::backtest::data::{load_historical_signals, LoadResult};
use crate::backtest::engine::{simulate_signal, CostAssumptions, SimulatedTrade};
use crate::backtest::split::{assign_splits, Split, SplitConfig};
use crate::backtest::stats::{compute_statistics, BacktestStatistics};
use crate::config::types::Config;
use serde::Deserialize;
use std::fmt;
use std::path::Path;

/// Top-level backtest configuration (loaded from TOML).
#[derive(Debug, Clone, Deserialize)]
pub struct BacktestConfig {
    /// Split boundaries.
    #[serde(default)]
    pub split: SplitConfig,
    /// Execution cost assumptions for modeling (all modeled, none observed).
    pub costs: CostAssumptions,
}

/// Complete backtest output.
pub struct BacktestOutput {
    pub train_stats: BacktestStatistics,
    pub validation_stats: BacktestStatistics,
    pub oos_stats: BacktestStatistics,
    pub all_trades: Vec<SimulatedTrade>,
    pub total_signals: usize,
    pub total_accepted: usize,
    pub total_rejected: usize,
    pub load_result: LoadResult,
}

impl fmt::Display for BacktestOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "========================================")?;
        writeln!(f, "         BACKTEST RESULTS")?;
        writeln!(f, "========================================")?;
        writeln!(f)?;
        writeln!(
            f,
            "Total signals: {} | Accepted: {} | Rejected: {}",
            self.total_signals, self.total_accepted, self.total_rejected
        )?;
        writeln!(f)?;

        writeln!(f, "--- TRAIN SET ---")?;
        writeln!(f, "{}", self.train_stats)?;
        writeln!(f)?;

        writeln!(f, "--- VALIDATION SET ---")?;
        writeln!(f, "{}", self.validation_stats)?;
        writeln!(f)?;

        writeln!(f, "--- OUT-OF-SAMPLE ---")?;
        writeln!(f, "{}", self.oos_stats)?;
        writeln!(f)?;

        if !self.load_result.rejection_reasons.is_empty() {
            writeln!(f, "--- DATA QUALITY ---")?;
            writeln!(
                f,
                "Records rejected during load: {}",
                self.load_result.rejected_count
            )?;
            for reason in &self.load_result.rejection_reasons {
                writeln!(f, "  - {reason}")?;
            }
        }
        Ok(())
    }
}

impl BacktestOutput {
    pub fn to_json_summary(&self) -> Result<String, String> {
        #[derive(serde::Serialize)]
        struct Summary<'a> {
            total_signals: usize,
            total_accepted: usize,
            total_rejected: usize,
            train: &'a BacktestStatistics,
            validation: &'a BacktestStatistics,
            out_of_sample: &'a BacktestStatistics,
        }
        let s = Summary {
            total_signals: self.total_signals,
            total_accepted: self.total_accepted,
            total_rejected: self.total_rejected,
            train: &self.train_stats,
            validation: &self.validation_stats,
            out_of_sample: &self.oos_stats,
        };
        serde_json::to_string_pretty(&s).map_err(|e| e.to_string())
    }
}

/// Run the full backtest pipeline.
pub fn run_backtest(
    config: &Config,
    bt_config: &BacktestConfig,
    input_path: &Path,
) -> Result<BacktestOutput, String> {
    let load_result = load_historical_signals(input_path)?;
    if load_result.signals.is_empty() {
        return Err("no valid historical signals in input file".into());
    }

    let total_signals_count = load_result.signals.len() + load_result.rejected_count;
    let total_rejected = load_result.rejected_count;
    let mut total_accepted = 0usize;

    let timestamps: Vec<_> = load_result
        .signals
        .iter()
        .map(|s| s.signal_timestamp)
        .collect();
    let splits = assign_splits(&timestamps, &bt_config.split);

    let mut all_trades = Vec::new();
    let mut train_trades = Vec::new();
    let mut val_trades = Vec::new();
    let mut oos_trades = Vec::new();

    for (signal, split) in load_result.signals.iter().zip(splits.iter()) {
        if let Ok(trade) = simulate_signal(signal, config, &bt_config.costs, split.clone()) {
            total_accepted += 1;
            match trade.split {
                Split::Train => train_trades.push(trade.clone()),
                Split::Validation => val_trades.push(trade.clone()),
                Split::OutOfSample => oos_trades.push(trade.clone()),
            }
            all_trades.push(trade);
        }
    }

    let train_stats = compute_statistics(
        &train_trades,
        train_trades.len() + total_rejected,
        total_rejected,
    );
    let val_stats = compute_statistics(&val_trades, val_trades.len(), 0);
    let oos_stats = compute_statistics(&oos_trades, oos_trades.len(), 0);

    Ok(BacktestOutput {
        train_stats,
        validation_stats: val_stats,
        oos_stats,
        all_trades,
        total_signals: total_signals_count,
        total_accepted,
        total_rejected,
        load_result,
    })
}
