use crate::backtest::engine::{CostMode, SimulatedTrade};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Outcome of the OOS statistical analysis.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum OosVerdict {
    /// OOS mean net return is positive with sufficient sample.
    PositiveExpectancy,
    /// OOS mean net return is negative with sufficient sample.
    NegativeExpectancy,
    /// OOS sample is too small for a meaningful conclusion.
    Inconclusive,
    /// No OOS data exists at all.
    NoOosData,
    /// The dataset is synthetic; no real-world conclusions should be drawn.
    SyntheticData,
}

impl std::fmt::Display for OosVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OosVerdict::PositiveExpectancy => write!(f, "POSITIVE_EXPECTANCY"),
            OosVerdict::NegativeExpectancy => write!(f, "NEGATIVE_EXPECTANCY"),
            OosVerdict::Inconclusive => write!(f, "INCONCLUSIVE"),
            OosVerdict::NoOosData => write!(f, "NO_OOS_DATA"),
            OosVerdict::SyntheticData => write!(f, "SYNTHETIC_DATA"),
        }
    }
}

/// Minimum number of non-ambiguous, non-censored OOS trades required
/// for a meaningful statistical conclusion.
const MIN_OOS_TRADES_FOR_VERDICT: usize = 5;

/// Backtest statistics for a set of trades.
///
/// All metrics are TRADE-LEVEL (not portfolio-level). They describe the
/// distribution of individual trade outcomes, not the equity curve of a
/// sequential portfolio simulation.
///
/// Ambiguous and censored trades are excluded from win/loss/return metrics
/// but counted separately. Costs are MODELED assumptions unless otherwise noted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestStatistics {
    /// Total signals evaluated (accepted + rejected at load time).
    pub total_signals: usize,
    /// Signals that passed strategy evaluation and produced a simulated trade.
    pub accepted_trades: usize,
    /// Signals rejected by strategy evaluation (included in total_signals).
    pub rejected_signals: usize,
    /// Trades where the interval was ambiguous (SL and TP both crossed).
    /// Excluded from win/loss/return statistics.
    pub ambiguous_trades: usize,
    /// Trades where the price history was insufficient to determine exit.
    /// Excluded from win/loss/return statistics.
    pub censored_trades: usize,
    /// Percentage of non-ambiguous, non-censored trades with net PnL > 0.
    /// Numerator: net_pnl > 0 trades. Denominator: all non-ambiguous, non-censored.
    pub win_rate: Decimal,
    /// Mean net return % of winning trades only (net PnL / position * 100).
    pub avg_win_pct: Decimal,
    /// Mean net return % of losing trades only (net PnL / position * 100).
    pub avg_loss_pct: Decimal,
    /// Median net return % across all non-ambiguous, non-censored trades.
    pub median_return_pct: Decimal,
    /// Mean net return % across all non-ambiguous, non-censored trades.
    /// Numerator: sum of net_return_pct. Denominator: count.
    pub expectancy_per_trade_pct: Decimal,
    /// Sum of net PnL from winning trades / |sum of net PnL from losing trades|.
    /// Both numerator and denominator use NET PnL (after modeled costs).
    /// When no losses exist and wins > 0, returns i32::MAX.
    pub profit_factor: Decimal,
    /// Sum of gross PnL across ALL trades (including ambiguous/censored).
    pub gross_pnl_usd: Decimal,
    /// Sum of net PnL across ALL trades (including ambiguous/censored).
    pub net_pnl_usd: Decimal,
    /// Sum of total modeled costs across ALL trades.
    pub total_costs_usd: Decimal,
    /// Maximum drawdown % from equity peak, computed from cumulative net PnL
    /// starting from starting_capital_usd. Excludes ambiguous/censored trades.
    pub max_drawdown_pct: Decimal,
    /// Longest consecutive sequence of trades with net_pnl <= 0.
    pub longest_losing_streak: usize,
    /// Mean holding time in minutes across ALL trades (including censored).
    pub avg_holding_minutes: Decimal,
    /// Sample standard deviation of net return % (Bessel-corrected).
    /// Only across non-ambiguous, non-censored trades.
    pub return_std_dev: Decimal,
    /// Standard error of the mean net return %.
    pub standard_error: Decimal,
    /// t-statistic: mean / standard_error. Non-ambiguous, non-censored only.
    pub t_statistic: Decimal,
    /// Sharpe-like ratio: mean / std_dev. Non-ambiguous, non-censored only.
    /// NOT annualized. This is a simplified trade-return metric.
    pub sharpe_like: Decimal,
    /// Sortino-like ratio: mean / downside_dev. Non-ambiguous, non-censored only.
    /// Downside deviation computed from negative returns only.
    pub sortino_like: Decimal,
    /// Whether costs are modeled, observed, or mixed.
    pub cost_mode: CostMode,
    /// Final OOS verdict based on statistical rules.
    pub oos_verdict: OosVerdict,
    /// Whether the dataset is synthetic (for empirical validity).
    pub is_synthetic_data: bool,
}

impl fmt::Display for BacktestStatistics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "=== Backtest Statistics ===")?;
        writeln!(f, "Cost mode:         {}", self.cost_mode)?;
        writeln!(
            f,
            "Data type:         {}",
            if self.is_synthetic_data {
                "SYNTHETIC"
            } else {
                "historical"
            }
        )?;
        writeln!(
            f,
            "Signals:           {} total, {} accepted, {} rejected",
            self.total_signals, self.accepted_trades, self.rejected_signals
        )?;
        writeln!(
            f,
            "Ambiguous:         {} (excluded from win/loss/return stats)",
            self.ambiguous_trades
        )?;
        writeln!(
            f,
            "Censored:          {} (excluded from win/loss/return stats)",
            self.censored_trades
        )?;
        writeln!(f)?;
        writeln!(f, "Win rate:          {}%", self.win_rate)?;
        writeln!(f, "Avg win:           {}%", self.avg_win_pct)?;
        writeln!(f, "Avg loss:          {}%", self.avg_loss_pct)?;
        writeln!(f, "Median return:     {}%", self.median_return_pct)?;
        writeln!(f, "Expectancy/trade:  {}%", self.expectancy_per_trade_pct)?;
        writeln!(f, "Profit factor:     {}", self.profit_factor)?;
        writeln!(f)?;
        writeln!(f, "Gross PnL:         ${}", self.gross_pnl_usd)?;
        writeln!(f, "Net PnL:           ${}", self.net_pnl_usd)?;
        writeln!(f, "Total costs:       ${}", self.total_costs_usd)?;
        writeln!(f, "Max drawdown:      {}%", self.max_drawdown_pct)?;
        writeln!(
            f,
            "Losing streak:     {} trades",
            self.longest_losing_streak
        )?;
        writeln!(f, "Avg holding:       {} min", self.avg_holding_minutes)?;
        writeln!(f)?;
        writeln!(f, "Return std dev:    {}%", self.return_std_dev)?;
        writeln!(f, "Standard error:    {}", self.standard_error)?;
        writeln!(f, "t-statistic:       {}", self.t_statistic)?;
        writeln!(f, "Sharpe-like:       {}", self.sharpe_like)?;
        writeln!(f, "Sortino-like:      {}", self.sortino_like)?;
        Ok(())
    }
}

/// Compute statistics for a set of trades.
///
/// Ambiguous and censored trades are excluded from win/loss/return statistics.
/// Max drawdown is computed from equity (starting_capital + cumulative net PnL).
pub fn compute_statistics(
    trades: &[SimulatedTrade],
    total_signals: usize,
    rejected_count: usize,
    starting_capital_usd: Decimal,
) -> BacktestStatistics {
    let accepted = trades.len();
    let ambiguous = trades.iter().filter(|t| t.is_ambiguous).count();
    let censored = trades.iter().filter(|t| t.is_censored).count();

    // Usable trades: non-ambiguous AND non-censored
    let usable: Vec<&SimulatedTrade> = trades
        .iter()
        .filter(|t| !t.is_ambiguous && !t.is_censored)
        .collect();

    let wins: Vec<&&SimulatedTrade> = usable
        .iter()
        .filter(|t| t.net_pnl_usd > Decimal::ZERO)
        .collect();
    let losses: Vec<&&SimulatedTrade> = usable
        .iter()
        .filter(|t| t.net_pnl_usd <= Decimal::ZERO)
        .collect();

    let n = Decimal::from(usable.len());
    let win_count = Decimal::from(wins.len());

    let win_rate = if n > Decimal::ZERO {
        win_count / n * dec!(100)
    } else {
        Decimal::ZERO
    };

    let avg_win_pct = if !wins.is_empty() {
        wins.iter().map(|t| t.net_return_pct).sum::<Decimal>() / Decimal::from(wins.len())
    } else {
        Decimal::ZERO
    };

    let avg_loss_pct = if !losses.is_empty() {
        losses.iter().map(|t| t.net_return_pct).sum::<Decimal>() / Decimal::from(losses.len())
    } else {
        Decimal::ZERO
    };

    let mut returns: Vec<Decimal> = usable.iter().map(|t| t.net_return_pct).collect();
    let median_return_pct = median(&mut returns);

    let expectancy = if n > Decimal::ZERO {
        usable.iter().map(|t| t.net_return_pct).sum::<Decimal>() / n
    } else {
        Decimal::ZERO
    };

    // Profit factor: sum of net PnL from wins / |sum of net PnL from losses|.
    // Both numerator and denominator use NET PnL (after modeled costs).
    let gross_wins: Decimal = wins.iter().map(|t| t.net_pnl_usd).sum();
    let gross_losses: Decimal = losses.iter().map(|t| t.net_pnl_usd.abs()).sum();
    let profit_factor = if gross_losses > Decimal::ZERO {
        gross_wins / gross_losses
    } else if gross_wins > Decimal::ZERO {
        Decimal::from(i32::MAX)
    } else {
        Decimal::ZERO
    };

    // Aggregate PnL includes ALL trades (including ambiguous/censored).
    let gross_pnl: Decimal = trades.iter().map(|t| t.gross_pnl_usd).sum();
    let net_pnl: Decimal = trades.iter().map(|t| t.net_pnl_usd).sum();
    let total_costs: Decimal = trades.iter().map(|t| t.total_cost_usd).sum();

    // Max drawdown: measured from equity peaks including starting capital.
    // equity_0 = starting_capital_usd
    // equity_i = equity_{i-1} + net_pnl_i (for usable trades only, in order)
    let mut equity = starting_capital_usd;
    let mut peak_equity = starting_capital_usd;
    let mut max_dd = Decimal::ZERO;
    for t in usable.iter() {
        equity += t.net_pnl_usd;
        if equity > peak_equity {
            peak_equity = equity;
        }
        let dd = if peak_equity > Decimal::ZERO {
            (peak_equity - equity) / peak_equity * dec!(100)
        } else {
            Decimal::ZERO
        };
        if dd > max_dd {
            max_dd = dd;
        }
    }

    let mut longest_streak = 0usize;
    let mut current_streak = 0usize;
    for t in usable.iter() {
        if t.net_pnl_usd <= Decimal::ZERO {
            current_streak += 1;
            if current_streak > longest_streak {
                longest_streak = current_streak;
            }
        } else {
            current_streak = 0;
        }
    }

    let avg_holding = if !trades.is_empty() {
        trades
            .iter()
            .map(|t| Decimal::from(t.holding_minutes))
            .sum::<Decimal>()
            / Decimal::from(trades.len())
    } else {
        Decimal::ZERO
    };

    let mean = if n > Decimal::ZERO {
        usable.iter().map(|t| t.net_return_pct).sum::<Decimal>() / n
    } else {
        Decimal::ZERO
    };
    let variance = if n > Decimal::ONE {
        usable
            .iter()
            .map(|t| {
                let d = t.net_return_pct - mean;
                d * d
            })
            .sum::<Decimal>()
            / (n - Decimal::ONE)
    } else {
        Decimal::ZERO
    };
    let std_dev = decimal_sqrt(&variance);

    let standard_error = if n > Decimal::ZERO {
        std_dev / decimal_sqrt(&n)
    } else {
        Decimal::ZERO
    };

    let t_statistic = if standard_error > Decimal::ZERO {
        mean / standard_error
    } else {
        Decimal::ZERO
    };

    let sharpe_like = if std_dev > Decimal::ZERO {
        mean / std_dev
    } else {
        Decimal::ZERO
    };

    let downside_var = if n > Decimal::ONE {
        usable
            .iter()
            .filter(|t| t.net_return_pct < Decimal::ZERO)
            .map(|t| t.net_return_pct * t.net_return_pct)
            .sum::<Decimal>()
            / n
    } else {
        Decimal::ZERO
    };
    let downside_dev = decimal_sqrt(&downside_var);
    let sortino_like = if downside_dev > Decimal::ZERO {
        mean / downside_dev
    } else {
        Decimal::ZERO
    };

    let cost_mode = trades
        .iter()
        .map(|t| t.cost_mode.clone())
        .next()
        .unwrap_or(CostMode::Modeled);

    BacktestStatistics {
        total_signals,
        accepted_trades: accepted,
        rejected_signals: rejected_count,
        ambiguous_trades: ambiguous,
        censored_trades: censored,
        win_rate,
        avg_win_pct,
        avg_loss_pct,
        median_return_pct,
        expectancy_per_trade_pct: expectancy,
        profit_factor,
        gross_pnl_usd: gross_pnl,
        net_pnl_usd: net_pnl,
        total_costs_usd: total_costs,
        max_drawdown_pct: max_dd,
        longest_losing_streak: longest_streak,
        avg_holding_minutes: avg_holding,
        return_std_dev: std_dev,
        standard_error,
        t_statistic,
        sharpe_like,
        sortino_like,
        cost_mode,
        oos_verdict: OosVerdict::Inconclusive,
        is_synthetic_data: true,
    }
}

/// Compute the OOS verdict based on statistical rules.
pub fn compute_oos_verdict(stats: &BacktestStatistics) -> OosVerdict {
    if stats.is_synthetic_data {
        return OosVerdict::SyntheticData;
    }
    if stats.accepted_trades == 0 && stats.censored_trades == 0 && stats.ambiguous_trades == 0 {
        return OosVerdict::NoOosData;
    }
    let usable_count = stats.accepted_trades - stats.ambiguous_trades - stats.censored_trades;
    if usable_count < MIN_OOS_TRADES_FOR_VERDICT {
        return OosVerdict::Inconclusive;
    }
    if stats.expectancy_per_trade_pct > Decimal::ZERO {
        OosVerdict::PositiveExpectancy
    } else {
        OosVerdict::NegativeExpectancy
    }
}

fn median(values: &mut [Decimal]) -> Decimal {
    if values.is_empty() {
        return Decimal::ZERO;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = values.len();
    if n.is_multiple_of(2) {
        (values[n / 2 - 1] + values[n / 2]) / dec!(2)
    } else {
        values[n / 2]
    }
}

fn decimal_sqrt(v: &Decimal) -> Decimal {
    if *v <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    let mut guess = *v / dec!(2);
    if guess == Decimal::ZERO {
        guess = dec!(1);
    }
    for _ in 0..50 {
        let next = (guess + *v / guess) / dec!(2);
        if (next - guess).abs() < dec!(0.0000000001) {
            return next;
        }
        guess = next;
    }
    guess
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backtest::engine::TradeCosts;
    use crate::backtest::split::Split;
    use crate::strategy::exit::ExitReason;

    fn make_trade(
        net_pnl: Decimal,
        return_pct: Decimal,
        holding: i64,
        ambiguous: bool,
        censored: bool,
    ) -> SimulatedTrade {
        SimulatedTrade {
            trade_id: "t1".into(),
            signal_timestamp: "2024-01-15T12:00:00Z".parse().unwrap(),
            mint: "mint1".into(),
            split: Split::Train,
            entry_time: "2024-01-15T12:00:00Z".parse().unwrap(),
            entry_price_usd: dec!(0.0001),
            position_usd: dec!(4),
            entry_quantity_tokens: dec!(40000),
            entry_costs: TradeCosts {
                swap_fee_usd: dec!(0.012),
                priority_fee_usd: dec!(0.002),
                slippage_cost_usd: dec!(0.02),
                price_impact_cost_usd: dec!(0.008),
                total_usd: dec!(0.042),
                is_observed: false,
            },
            exit_time: "2024-01-15T14:00:00Z".parse().unwrap(),
            exit_price_usd: dec!(0.00011),
            exit_reason: if censored {
                ExitReason::Censored
            } else {
                ExitReason::TakeProfit
            },
            holding_minutes: holding,
            exit_costs: TradeCosts {
                swap_fee_usd: dec!(0.0132),
                priority_fee_usd: dec!(0.002),
                slippage_cost_usd: dec!(0.022),
                price_impact_cost_usd: dec!(0.0088),
                total_usd: dec!(0.046),
                is_observed: false,
            },
            gross_return_pct: return_pct + dec!(1),
            gross_pnl_usd: net_pnl + dec!(0.088),
            total_cost_usd: dec!(0.088),
            net_return_pct: return_pct,
            net_pnl_usd: net_pnl,
            mfe_pct: dec!(10),
            mae_pct: dec!(-2),
            is_ambiguous: ambiguous,
            ambiguous_reason: None,
            is_censored: censored,
            censored_reason: if censored { Some("test".into()) } else { None },
            cost_mode: CostMode::Modeled,
        }
    }

    #[test]
    fn empty_trades() {
        let stats = compute_statistics(&[], 10, 5, dec!(100));
        assert_eq!(stats.total_signals, 10);
        assert_eq!(stats.accepted_trades, 0);
        assert_eq!(stats.rejected_signals, 5);
        assert_eq!(stats.win_rate, Decimal::ZERO);
    }

    #[test]
    fn win_rate_computed_correctly() {
        let trades = vec![
            make_trade(dec!(1), dec!(10), 30, false, false),
            make_trade(dec!(1), dec!(10), 30, false, false),
            make_trade(dec!(-1), dec!(-10), 30, false, false),
            make_trade(dec!(-1), dec!(-10), 30, false, false),
        ];
        let stats = compute_statistics(&trades, 4, 0, dec!(100));
        assert_eq!(stats.win_rate, dec!(50));
    }

    #[test]
    fn ambiguous_trades_excluded_from_win_rate() {
        let trades = vec![
            make_trade(dec!(1), dec!(10), 30, false, false),
            make_trade(dec!(1), dec!(10), 30, false, false),
            make_trade(dec!(-1), dec!(-10), 30, true, false),
        ];
        let stats = compute_statistics(&trades, 3, 0, dec!(100));
        assert_eq!(stats.ambiguous_trades, 1);
        assert_eq!(stats.win_rate, dec!(100));
    }

    #[test]
    fn censored_trades_excluded_from_win_rate() {
        let trades = vec![
            make_trade(dec!(1), dec!(10), 30, false, false),
            make_trade(dec!(1), dec!(10), 30, false, false),
            make_trade(dec!(0), dec!(0), 30, false, true),
        ];
        let stats = compute_statistics(&trades, 3, 0, dec!(100));
        assert_eq!(stats.censored_trades, 1);
        assert_eq!(stats.win_rate, dec!(100));
    }

    #[test]
    fn max_drawdown_with_starting_capital() {
        let trades = vec![
            make_trade(dec!(2), dec!(20), 30, false, false),
            make_trade(dec!(-3), dec!(-30), 30, false, false),
            make_trade(dec!(1), dec!(10), 30, false, false),
        ];
        let stats = compute_statistics(&trades, 3, 0, dec!(100));
        // equity: 100 → 102 → 99 → 100
        // peak: 102, drawdown from 102 to 99 = 3/102 ≈ 2.94%
        assert!(stats.max_drawdown_pct > Decimal::ZERO);
        assert!(stats.max_drawdown_pct < dec!(5));
    }

    #[test]
    fn max_drawdown_immediate_loss() {
        let trades = vec![
            make_trade(dec!(-10), dec!(-50), 30, false, false),
            make_trade(dec!(5), dec!(25), 30, false, false),
        ];
        let stats = compute_statistics(&trades, 2, 0, dec!(100));
        // equity: 100 → 90 → 95
        // peak: 100, drawdown = 10/100 = 10%
        assert_eq!(stats.max_drawdown_pct, dec!(10));
    }

    #[test]
    fn max_drawdown_multiple_peaks() {
        let trades = vec![
            make_trade(dec!(5), dec!(50), 30, false, false),
            make_trade(dec!(-3), dec!(-30), 30, false, false),
            make_trade(dec!(5), dec!(50), 30, false, false),
            make_trade(dec!(-4), dec!(-40), 30, false, false),
            make_trade(dec!(3), dec!(30), 30, false, false),
        ];
        let stats = compute_statistics(&trades, 5, 0, dec!(100));
        // equity: 100 → 105 → 102 → 107 → 103 → 106
        // peaks: 105, 107
        // dd from 105 to 102 = 3/105 ≈ 2.86%
        // dd from 107 to 103 = 4/107 ≈ 3.74%
        assert!(stats.max_drawdown_pct > dec!(3));
        assert!(stats.max_drawdown_pct < dec!(4));
    }

    #[test]
    fn longest_losing_streak_counted() {
        let trades = vec![
            make_trade(dec!(1), dec!(10), 30, false, false),
            make_trade(dec!(-1), dec!(-10), 30, false, false),
            make_trade(dec!(-1), dec!(-10), 30, false, false),
            make_trade(dec!(-1), dec!(-10), 30, false, false),
            make_trade(dec!(1), dec!(10), 30, false, false),
        ];
        let stats = compute_statistics(&trades, 5, 0, dec!(100));
        assert_eq!(stats.longest_losing_streak, 3);
    }

    #[test]
    fn profit_factor_uses_net_pnl_consistently() {
        let trades = vec![
            make_trade(dec!(2), dec!(20), 30, false, false),
            make_trade(dec!(1), dec!(10), 30, false, false),
            make_trade(dec!(-1), dec!(-10), 30, false, false),
        ];
        let stats = compute_statistics(&trades, 3, 0, dec!(100));
        // gross_wins = 2 + 1 = 3, gross_losses = |-1| = 1
        assert_eq!(stats.profit_factor, dec!(3));
    }

    #[test]
    fn total_costs_summed() {
        let trades = vec![make_trade(dec!(0), dec!(0), 30, false, false)];
        let stats = compute_statistics(&trades, 1, 0, dec!(100));
        assert_eq!(stats.total_costs_usd, dec!(0.088));
    }

    #[test]
    fn holding_time_averaged() {
        let trades = vec![
            make_trade(dec!(1), dec!(10), 30, false, false),
            make_trade(dec!(1), dec!(10), 60, false, false),
        ];
        let stats = compute_statistics(&trades, 2, 0, dec!(100));
        assert_eq!(stats.avg_holding_minutes, dec!(45));
    }

    #[test]
    fn t_statistic_nonzero_with_data() {
        let trades = vec![
            make_trade(dec!(1), dec!(10), 30, false, false),
            make_trade(dec!(2), dec!(20), 30, false, false),
            make_trade(dec!(-1), dec!(-10), 30, false, false),
        ];
        let stats = compute_statistics(&trades, 3, 0, dec!(100));
        assert!(stats.t_statistic > Decimal::ZERO);
    }

    #[test]
    fn sharpe_like_zero_when_no_variance() {
        let trades = vec![
            make_trade(dec!(1), dec!(10), 30, false, false),
            make_trade(dec!(1), dec!(10), 30, false, false),
        ];
        let stats = compute_statistics(&trades, 2, 0, dec!(100));
        assert_eq!(stats.sharpe_like, Decimal::ZERO);
    }

    #[test]
    fn oos_verdict_synthetic() {
        let mut stats = BacktestStatistics {
            total_signals: 10,
            accepted_trades: 8,
            rejected_signals: 2,
            ambiguous_trades: 0,
            censored_trades: 0,
            win_rate: dec!(60),
            avg_win_pct: dec!(10),
            avg_loss_pct: dec!(-5),
            median_return_pct: dec!(5),
            expectancy_per_trade_pct: dec!(3),
            profit_factor: dec!(2),
            gross_pnl_usd: dec!(20),
            net_pnl_usd: dec!(15),
            total_costs_usd: dec!(5),
            max_drawdown_pct: dec!(10),
            longest_losing_streak: 2,
            avg_holding_minutes: dec!(30),
            return_std_dev: dec!(8),
            standard_error: dec!(3),
            t_statistic: dec!(1),
            sharpe_like: dec!(0.5),
            sortino_like: dec!(0.7),
            cost_mode: CostMode::Modeled,
            oos_verdict: OosVerdict::Inconclusive,
            is_synthetic_data: true,
        };
        stats.oos_verdict = compute_oos_verdict(&stats);
        assert_eq!(stats.oos_verdict, OosVerdict::SyntheticData);
    }

    #[test]
    fn oos_verdict_inconclusive_when_too_few_trades() {
        let stats = BacktestStatistics {
            total_signals: 3,
            accepted_trades: 2,
            rejected_signals: 1,
            ambiguous_trades: 0,
            censored_trades: 0,
            win_rate: dec!(50),
            avg_win_pct: dec!(10),
            avg_loss_pct: dec!(-5),
            median_return_pct: dec!(2.5),
            expectancy_per_trade_pct: dec!(2.5),
            profit_factor: dec!(2),
            gross_pnl_usd: dec!(5),
            net_pnl_usd: dec!(4),
            total_costs_usd: dec!(1),
            max_drawdown_pct: dec!(5),
            longest_losing_streak: 1,
            avg_holding_minutes: dec!(30),
            return_std_dev: dec!(8),
            standard_error: dec!(5),
            t_statistic: dec!(0.5),
            sharpe_like: dec!(0.3),
            sortino_like: dec!(0.4),
            cost_mode: CostMode::Modeled,
            oos_verdict: OosVerdict::Inconclusive,
            is_synthetic_data: false,
        };
        let verdict = compute_oos_verdict(&stats);
        assert_eq!(verdict, OosVerdict::Inconclusive);
    }
}
