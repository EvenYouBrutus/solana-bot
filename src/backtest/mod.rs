pub mod data;
pub mod engine;
pub mod split;
pub mod stats;

pub use engine::{BacktestConfig, BacktestResult, CostAssumptions};
pub use split::Split;
pub use stats::{compute_statistics, BacktestStatistics, OosVerdict};

use crate::config::types::Config;
use std::path::Path;

/// Run the full backtest pipeline: load → validate → simulate → stats.
///
/// This is the single entry point called from `main.rs`.
///
/// Fail-closed: an invalid split configuration aborts the run instead of
/// silently assigning records to Train. Records are explicitly separated
/// into malformed, structurally rejected, strategy-rejected (with
/// structured reasons), range-excluded, and accepted trades.
pub fn run_backtest(
    config: &Config,
    bt_config: &BacktestConfig,
    input_path: &Path,
) -> Result<BacktestResult, String> {
    // Fail-closed boundary validation BEFORE any record is classified:
    // train_end <= validation_start < validation_end <= oos_start
    // (for every pair of boundaries that are configured).
    bt_config.split.validate_boundaries()?;

    let load_result = data::load_historical_signals(input_path)?;
    // Malformed records: unparseable JSON or records failing point-in-time /
    // structural validation at load time.
    let malformed_records = load_result.rejection_reasons;
    let total_signals_before = load_result.signals.len() + load_result.rejected_count;
    let mut signals = load_result.signals;
    let (accepted, structural_rejections) = data::prefilter_signals(&mut signals);

    let cost_assumptions = CostAssumptions::from_config(bt_config);

    let mut trades = Vec::new();
    let mut strategy_rejections: Vec<data::SignalRejection> = Vec::new();
    let mut range_excluded: Vec<data::SignalRejection> = Vec::new();
    let mut trade_index = 0usize;

    for signal in &accepted {
        let (split, exclusion) =
            split::classify_split_with_exclusion(signal.signal_timestamp, &bt_config.split);
        // Signals outside the configured experiment range are explicitly
        // reported as excluded — never silently simulated (and never
        // silently dumped into Train).
        if let Some(reason) = exclusion {
            range_excluded.push(data::SignalRejection {
                reason: format!("outside experiment range: {reason}"),
                mint: signal.mint.clone(),
                signal_timestamp: signal.signal_timestamp.to_rfc3339(),
            });
            continue;
        }

        match engine::simulate_signal(signal, config, &cost_assumptions, split, trade_index) {
            Ok(trade) => {
                trades.push(trade);
                trade_index += 1;
            }
            Err(reason) => {
                // Strategy rejection: retain the structured reason.
                strategy_rejections.push(data::SignalRejection {
                    reason,
                    mint: signal.mint.clone(),
                    signal_timestamp: signal.signal_timestamp.to_rfc3339(),
                });
            }
        }
    }

    // Trades are simulated in signal_timestamp order (the loader sorts), so
    // the equity path used for drawdown is chronological.
    let mut stats = compute_statistics(
        &trades,
        total_signals_before,
        structural_rejections.len(),
        bt_config.capital_usd,
    );

    // OOS evaluation: verdict, sample size, mean, and 95% CI are computed
    // from the OOS split ONLY — never from the full (train-contaminated)
    // sample.
    let oos_trades: Vec<&engine::SimulatedTrade> = trades
        .iter()
        .filter(|t| t.split == Split::OutOfSample)
        .collect();
    let oos_owned: Vec<engine::SimulatedTrade> = oos_trades.into_iter().cloned().collect();
    let oos_stats = compute_statistics(&oos_owned, oos_owned.len(), 0, bt_config.capital_usd);
    stats.oos_verdict = stats::compute_oos_verdict(&oos_stats);
    stats.oos_sample_size = oos_stats.accepted_trades;
    stats.oos_mean_return_pct = oos_stats.expectancy_per_trade_pct;
    let (ci_lo, ci_hi) = stats::ci95(oos_stats.expectancy_per_trade_pct, oos_stats.standard_error);
    stats.oos_ci95_lower_pct = ci_lo;
    stats.oos_ci95_upper_pct = ci_hi;

    Ok(BacktestResult {
        statistics: stats,
        all_trades: trades,
        total_signals: total_signals_before,
        accepted_trades: accepted.len(),
        rejected_count: structural_rejections.len(),
        malformed_records,
        structural_rejections,
        strategy_rejections,
        range_excluded,
    })
}
