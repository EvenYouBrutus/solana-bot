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
pub fn run_backtest(
    config: &Config,
    bt_config: &BacktestConfig,
    input_path: &Path,
) -> Result<BacktestResult, String> {
    let load_result = data::load_historical_signals(input_path)?;
    let total_signals_before = load_result.signals.len() + load_result.rejected_count;
    let mut signals = load_result.signals;
    let (accepted, rejected) = data::prefilter_signals(&mut signals);
    let rejected_count = rejected.len();

    let cost_assumptions = CostAssumptions::from_config(bt_config);

    let mut trades = Vec::new();
    let mut trade_index = 0usize;

    for signal in &accepted {
        let split =
            split::classify_split_with_exclusion(signal.signal_timestamp, &bt_config.split).0;

        match engine::simulate_signal(signal, config, &cost_assumptions, split, trade_index) {
            Ok(trade) => {
                trades.push(trade);
                trade_index += 1;
            }
            Err(e) => {
                eprintln!("simulate_signal failed for {}: {e}", signal.mint);
            }
        }
    }

    let stats = compute_statistics(
        &trades,
        total_signals_before,
        rejected_count,
        bt_config.capital_usd,
    );

    Ok(BacktestResult {
        statistics: stats,
        all_trades: trades,
        total_signals: total_signals_before,
        accepted_trades: accepted.len(),
        rejected_count,
    })
}
