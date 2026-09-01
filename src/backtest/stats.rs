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
    /// Sufficient sample but no statistical dispersion to test against
    /// (e.g. every usable trade returned identically).
    Inconclusive,
    /// No OOS data exists at all.
    NoOosData,
    /// OOS trades exist but every outcome is ambiguous or censored: the
    /// realized OOS result is unknowable from this data.
    InvalidData,
    /// Usable OOS sample is below the minimum required for any conclusion.
    /// A tiny positive sample is NOT evidence of profitability.
    InsufficientOosSample,
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
            OosVerdict::InvalidData => write!(f, "INVALID_DATA"),
            OosVerdict::InsufficientOosSample => write!(f, "INSUFFICIENT_OOS_SAMPLE"),
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
    /// Maximum drawdown % from equity peak. Equity is the chronological path
    /// starting_capital_usd + cumulative net PnL over usable trades in trade
    /// (signal-timestamp) order; the actual configured starting capital is
    /// the anchor. Excludes ambiguous/censored trades.
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
    /// NOT annualized (per-trade horizon); it is a simplified trade-return
    /// metric and must not be compared against annualized Sharpe values.
    pub sharpe_like: Decimal,
    /// Sortino-like ratio: mean / downside_dev. Non-ambiguous, non-censored
    /// only. NOT annualized. Downside deviation squares only negative
    /// returns but divides by ALL usable trades (target-zero convention).
    pub sortino_like: Decimal,
    /// Whether costs are modeled, observed, or mixed.
    pub cost_mode: CostMode,
    /// Total OOS trades BEFORE excluding ambiguous/censored outcomes.
    /// This is the number of simulated OOS trades, not the statistical
    /// sample size.
    pub oos_total_trades: usize,
    /// Number of OOS trades with a fully realized outcome (non-ambiguous
    /// AND non-censored). This IS the statistical sample size used for
    /// the mean, std dev, standard error, confidence interval, and
    /// verdict. Must be excluded from any "sample size" claim otherwise.
    pub oos_usable_trades: usize,
    /// Number of OOS trades excluded because the outcome was ambiguous.
    pub oos_ambiguous_trades: usize,
    /// Number of OOS trades excluded because the price history was
    /// insufficient to determine a valid exit.
    pub oos_censored_trades: usize,
    /// Mean net return % across USABLE (non-ambiguous, non-censored) OOS
    /// trades only.
    pub oos_mean_return_pct: Decimal,
    /// Lower bound of the 95% confidence interval for the OOS mean net
    /// return: mean - 1.96 * standard_error (normal approximation).
    pub oos_ci95_lower_pct: Decimal,
    /// Upper bound of the 95% confidence interval for the OOS mean net
    /// return: mean + 1.96 * standard_error (normal approximation).
    pub oos_ci95_upper_pct: Decimal,
    /// Final OOS verdict based on statistical rules.
    pub oos_verdict: OosVerdict,
    /// Whether the dataset is synthetic (operator-set flag from
    /// `BacktestConfig.is_synthetic_data`; never inferred from results).
    /// When `true`, the verdict is forced to `SYNTHETIC_DATA`.
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
        writeln!(
            f,
            "Sharpe-like:       {} (per trade, not annualized)",
            self.sharpe_like
        )?;
        writeln!(
            f,
            "Sortino-like:      {} (per trade, not annualized)",
            self.sortino_like
        )?;
        writeln!(f)?;
        writeln!(
            f,
            "OOS total trades:  {} (all simulated OOS trades, before exclusions)",
            self.oos_total_trades
        )?;
        writeln!(
            f,
            "OOS usable:        {} (statistical sample: non-ambiguous, non-censored)",
            self.oos_usable_trades
        )?;
        writeln!(
            f,
            "OOS ambiguous:     {} (excluded from mean, CI, and verdict)",
            self.oos_ambiguous_trades
        )?;
        writeln!(
            f,
            "OOS censored:      {} (excluded from mean, CI, and verdict)",
            self.oos_censored_trades
        )?;
        writeln!(f, "OOS mean return:   {}%", self.oos_mean_return_pct)?;
        writeln!(
            f,
            "OOS 95% CI:        [{}, {}] (normal approx)",
            self.oos_ci95_lower_pct, self.oos_ci95_upper_pct
        )?;
        writeln!(f, "OOS verdict:       {}", self.oos_verdict)?;
        Ok(())
    }
}

/// Compute statistics for a set of trades.
///
/// Ambiguous and censored trades are excluded from win/loss/return statistics.
/// Max drawdown is computed from equity (starting_capital + cumulative net PnL).
///
/// `is_synthetic_data` is the operator-set flag identifying whether the
/// input dataset is synthetic. It MUST be passed in explicitly; the
/// statistics layer MUST NOT infer it from results.
pub fn compute_statistics(
    trades: &[SimulatedTrade],
    total_signals: usize,
    rejected_count: usize,
    starting_capital_usd: Decimal,
    is_synthetic_data: bool,
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
        oos_total_trades: 0,
        oos_usable_trades: 0,
        oos_ambiguous_trades: 0,
        oos_censored_trades: 0,
        oos_mean_return_pct: Decimal::ZERO,
        oos_ci95_lower_pct: Decimal::ZERO,
        oos_ci95_upper_pct: Decimal::ZERO,
        oos_verdict: OosVerdict::Inconclusive,
        is_synthetic_data,
    }
}

/// 95% confidence interval for a mean given its standard error, using the
/// normal approximation (mean ± 1.96·SE). Valid for large samples; for small
/// samples it is narrower than an exact t-interval (conservative reading).
pub fn ci95(mean: Decimal, standard_error: Decimal) -> (Decimal, Decimal) {
    let half = dec!(1.96) * standard_error;
    (mean - half, mean + half)
}

/// Compute the OOS verdict from OOS-ONLY statistics.
///
/// Decision order (fail-closed, CI-aware):
/// 1. synthetic dataset → `SyntheticData` (no real-world conclusions);
/// 2. no OOS trades at all → `NoOosData`;
/// 3. OOS trades exist but all are ambiguous/censored → `InvalidData`
///    (the realized outcome is unknowable);
/// 4. usable sample < `MIN_OOS_TRADES_FOR_VERDICT` → `InsufficientOosSample`
///    (a tiny positive sample is NOT evidence of profitability);
/// 5. 95% CI for the mean return crosses 0 → `Inconclusive`
///    (a positive mean is NOT enough; the interval must be wholly above 0);
/// 6. CI lower bound strictly > 0 → `PositiveExpectancy`;
/// 7. CI upper bound strictly < 0 → `NegativeExpectancy`.
pub fn compute_oos_verdict(stats: &BacktestStatistics) -> OosVerdict {
    if stats.is_synthetic_data {
        return OosVerdict::SyntheticData;
    }
    if stats.oos_total_trades == 0 {
        return OosVerdict::NoOosData;
    }
    if stats.oos_usable_trades == 0 {
        // Trades exist, but every OOS outcome is ambiguous or censored.
        return OosVerdict::InvalidData;
    }
    if stats.oos_usable_trades < MIN_OOS_TRADES_FOR_VERDICT {
        return OosVerdict::InsufficientOosSample;
    }
    // If the 95% CI for the mean return includes 0, the result is not
    // statistically distinguishable from zero. Never claim positive or
    // negative expectancy on a CI that crosses zero.
    if stats.oos_ci95_lower_pct <= Decimal::ZERO && stats.oos_ci95_upper_pct >= Decimal::ZERO {
        return OosVerdict::Inconclusive;
    }
    if stats.oos_ci95_lower_pct > Decimal::ZERO {
        return OosVerdict::PositiveExpectancy;
    }
    if stats.oos_ci95_upper_pct < Decimal::ZERO {
        return OosVerdict::NegativeExpectancy;
    }
    // Unreachable given the CI check above, but keep a safe fallback.
    OosVerdict::Inconclusive
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
        let stats = compute_statistics(&[], 10, 5, dec!(100), false);
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
        let stats = compute_statistics(&trades, 4, 0, dec!(100), false);
        assert_eq!(stats.win_rate, dec!(50));
    }

    #[test]
    fn ambiguous_trades_excluded_from_win_rate() {
        let trades = vec![
            make_trade(dec!(1), dec!(10), 30, false, false),
            make_trade(dec!(1), dec!(10), 30, false, false),
            make_trade(dec!(-1), dec!(-10), 30, true, false),
        ];
        let stats = compute_statistics(&trades, 3, 0, dec!(100), false);
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
        let stats = compute_statistics(&trades, 3, 0, dec!(100), false);
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
        let stats = compute_statistics(&trades, 3, 0, dec!(100), false);
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
        let stats = compute_statistics(&trades, 2, 0, dec!(100), false);
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
        let stats = compute_statistics(&trades, 5, 0, dec!(100), false);
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
        let stats = compute_statistics(&trades, 5, 0, dec!(100), false);
        assert_eq!(stats.longest_losing_streak, 3);
    }

    #[test]
    fn profit_factor_uses_net_pnl_consistently() {
        let trades = vec![
            make_trade(dec!(2), dec!(20), 30, false, false),
            make_trade(dec!(1), dec!(10), 30, false, false),
            make_trade(dec!(-1), dec!(-10), 30, false, false),
        ];
        let stats = compute_statistics(&trades, 3, 0, dec!(100), false);
        // gross_wins = 2 + 1 = 3, gross_losses = |-1| = 1
        assert_eq!(stats.profit_factor, dec!(3));
    }

    #[test]
    fn total_costs_summed() {
        let trades = vec![make_trade(dec!(0), dec!(0), 30, false, false)];
        let stats = compute_statistics(&trades, 1, 0, dec!(100), false);
        assert_eq!(stats.total_costs_usd, dec!(0.088));
    }

    #[test]
    fn holding_time_averaged() {
        let trades = vec![
            make_trade(dec!(1), dec!(10), 30, false, false),
            make_trade(dec!(1), dec!(10), 60, false, false),
        ];
        let stats = compute_statistics(&trades, 2, 0, dec!(100), false);
        assert_eq!(stats.avg_holding_minutes, dec!(45));
    }

    #[test]
    fn t_statistic_nonzero_with_data() {
        let trades = vec![
            make_trade(dec!(1), dec!(10), 30, false, false),
            make_trade(dec!(2), dec!(20), 30, false, false),
            make_trade(dec!(-1), dec!(-10), 30, false, false),
        ];
        let stats = compute_statistics(&trades, 3, 0, dec!(100), false);
        assert!(stats.t_statistic > Decimal::ZERO);
    }

    #[test]
    fn standard_error_is_std_dev_over_sqrt_n() {
        // Hand-checkable dispersion: returns 10, 20, 30 → mean 20,
        // sample variance = (100+0+100)/2 = 100 → std_dev 10, n = 3,
        // SE = 10 / sqrt(3) ≈ 5.7735.
        let trades = vec![
            make_trade(dec!(1), dec!(10), 30, false, false),
            make_trade(dec!(2), dec!(20), 30, false, false),
            make_trade(dec!(3), dec!(30), 30, false, false),
        ];
        let stats = compute_statistics(&trades, 3, 0, dec!(100), false);
        let expected = dec!(10) / decimal_sqrt(&Decimal::from(3usize));
        assert!((stats.standard_error - expected).abs() < dec!(0.0001));
        // t = mean / SE
        assert!((stats.t_statistic - dec!(20) / expected).abs() < dec!(0.001));
    }

    #[test]
    fn ci95_is_mean_plus_minus_196_se() {
        let (lo, hi) = ci95(dec!(2), dec!(1));
        assert_eq!(lo, dec!(0.04));
        assert_eq!(hi, dec!(3.96));
        // Zero SE collapses to the point estimate.
        let (lo, hi) = ci95(dec!(2), Decimal::ZERO);
        assert_eq!(lo, dec!(2));
        assert_eq!(hi, dec!(2));
    }

    #[test]
    fn ci_brackets_mean_when_computed_from_trades() {
        let trades = vec![
            make_trade(dec!(1), dec!(10), 30, false, false),
            make_trade(dec!(2), dec!(20), 30, false, false),
            make_trade(dec!(-1), dec!(-10), 30, false, false),
        ];
        let stats = compute_statistics(&trades, 3, 0, dec!(100), false);
        let (lo, hi) = ci95(stats.expectancy_per_trade_pct, stats.standard_error);
        assert!(lo < stats.expectancy_per_trade_pct);
        assert!(hi > stats.expectancy_per_trade_pct);
    }

    #[test]
    fn sharpe_like_zero_when_no_variance() {
        let trades = vec![
            make_trade(dec!(1), dec!(10), 30, false, false),
            make_trade(dec!(1), dec!(10), 30, false, false),
        ];
        let stats = compute_statistics(&trades, 2, 0, dec!(100), false);
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
            oos_total_trades: 8,
            oos_usable_trades: 8,
            oos_ambiguous_trades: 0,
            oos_censored_trades: 0,
            oos_mean_return_pct: dec!(3),
            oos_ci95_lower_pct: dec!(-2.88),
            oos_ci95_upper_pct: dec!(8.88),
            oos_verdict: OosVerdict::Inconclusive,
            is_synthetic_data: true,
        };
        stats.oos_verdict = compute_oos_verdict(&stats);
        assert_eq!(stats.oos_verdict, OosVerdict::SyntheticData);
    }

    #[test]
    fn oos_verdict_insufficient_sample_when_too_few_trades() {
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
            oos_total_trades: 2,
            oos_usable_trades: 2,
            oos_ambiguous_trades: 0,
            oos_censored_trades: 0,
            oos_mean_return_pct: dec!(2.5),
            oos_ci95_lower_pct: dec!(-7.3),
            oos_ci95_upper_pct: dec!(12.3),
            oos_verdict: OosVerdict::Inconclusive,
            is_synthetic_data: false,
        };
        let verdict = compute_oos_verdict(&stats);
        assert_eq!(verdict, OosVerdict::InsufficientOosSample);
    }

    #[test]
    fn tiny_positive_oos_sample_is_not_profitability_evidence() {
        // One glowing OOS trade must NOT yield POSITIVE_EXPECTANCY.
        let trades = vec![make_trade(dec!(5), dec!(50), 30, false, false)];
        let stats = compute_statistics(&trades, 1, 0, dec!(100), false);
        // The verdict function reads the OOS sample fields, which are
        // populated by the pipeline. In a unit test we populate them
        // manually.
        let verdict = with_oos_fields(&stats, 1, 1, 0, 0);
        assert_eq!(verdict, OosVerdict::InsufficientOosSample);
    }

    #[test]
    fn oos_verdict_invalid_data_when_all_outcomes_unknowable() {
        // OOS trades exist, but every one is ambiguous or censored: the
        // realized OOS outcome cannot be known from this data.
        let trades = vec![
            make_trade(dec!(0), dec!(0), 30, true, false),
            make_trade(dec!(0), dec!(0), 30, false, true),
        ];
        let stats = compute_statistics(&trades, 2, 0, dec!(100), false);
        let verdict = with_oos_fields(&stats, 2, 0, 1, 1);
        assert_eq!(verdict, OosVerdict::InvalidData);
    }

    #[test]
    fn oos_verdict_no_data() {
        let stats = compute_statistics(&[], 0, 0, dec!(100), false);
        assert_eq!(compute_oos_verdict(&stats), OosVerdict::NoOosData);
    }

    #[test]
    fn oos_verdict_positive_when_ci_entirely_above_zero() {
        // 6 wins (30%) + 1 small loss (-5%) → mean ≈ 25%, std_dev small
        // enough that CI lower bound > 0.
        let mut trades = Vec::new();
        for _ in 0..6 {
            trades.push(make_trade(dec!(2), dec!(30), 30, false, false));
        }
        trades.push(make_trade(dec!(-0.2), dec!(-5), 30, false, false));
        let stats = compute_statistics(&trades, 7, 0, dec!(100), false);
        // Compute the CI from the stats' mean + SE (the pipeline does
        // this in `run_backtest`).
        let (ci_lo, _ci_hi) = ci95(stats.expectancy_per_trade_pct, stats.standard_error);
        let mut s = stats.clone();
        s.oos_total_trades = 7;
        s.oos_usable_trades = 7;
        s.oos_ambiguous_trades = 0;
        s.oos_censored_trades = 0;
        s.oos_ci95_lower_pct = ci_lo;
        s.oos_ci95_upper_pct = _ci_hi;
        assert!(s.expectancy_per_trade_pct > Decimal::ZERO);
        assert!(s.oos_ci95_lower_pct > Decimal::ZERO);
        assert_eq!(compute_oos_verdict(&s), OosVerdict::PositiveExpectancy);
    }

    #[test]
    fn oos_verdict_inconclusive_when_ci_crosses_zero() {
        // 6 wins (5%) + 1 large loss (-20%) → mean ~1.4% with very high
        // variance. CI crosses 0 → Inconclusive, NOT Positive.
        let mut trades = Vec::new();
        for _ in 0..6 {
            trades.push(make_trade(dec!(0.5), dec!(5), 30, false, false));
        }
        trades.push(make_trade(dec!(-2), dec!(-20), 30, false, false));
        let stats = compute_statistics(&trades, 7, 0, dec!(100), false);
        let (ci_lo, ci_hi) = ci95(stats.expectancy_per_trade_pct, stats.standard_error);
        let mut s = stats.clone();
        s.oos_total_trades = 7;
        s.oos_usable_trades = 7;
        s.oos_ambiguous_trades = 0;
        s.oos_censored_trades = 0;
        s.oos_ci95_lower_pct = ci_lo;
        s.oos_ci95_upper_pct = ci_hi;
        assert!(s.expectancy_per_trade_pct > Decimal::ZERO);
        assert!(s.oos_ci95_lower_pct < Decimal::ZERO);
        assert!(s.oos_ci95_upper_pct > Decimal::ZERO);
        assert_eq!(compute_oos_verdict(&s), OosVerdict::Inconclusive);
    }

    #[test]
    fn oos_verdict_zero_dispersion_collapsed_ci_is_positive() {
        // With zero SE, the CI collapses to the point estimate. If the
        // point estimate is > 0, the lower bound is also > 0 and the
        // verdict is PositiveExpectancy (a degenerate but consistent
        // case — the data says every trade returned identically positive).
        let trades: Vec<_> = (0..6)
            .map(|_| make_trade(dec!(1), dec!(10), 30, false, false))
            .collect();
        let stats = compute_statistics(&trades, 6, 0, dec!(100), false);
        let (ci_lo, ci_hi) = ci95(stats.expectancy_per_trade_pct, stats.standard_error);
        let mut s = stats.clone();
        s.oos_total_trades = 6;
        s.oos_usable_trades = 6;
        s.oos_ambiguous_trades = 0;
        s.oos_censored_trades = 0;
        s.oos_ci95_lower_pct = ci_lo;
        s.oos_ci95_upper_pct = ci_hi;
        assert_eq!(s.standard_error, Decimal::ZERO);
        assert!(s.oos_ci95_lower_pct > Decimal::ZERO);
        assert_eq!(compute_oos_verdict(&s), OosVerdict::PositiveExpectancy);
    }

    #[test]
    fn sortino_like_zero_when_no_negative_returns() {
        // All wins: downside_var = 0 → downside_dev = 0 → sortino = 0.
        let trades = vec![
            make_trade(dec!(1), dec!(10), 30, false, false),
            make_trade(dec!(2), dec!(20), 30, false, false),
        ];
        let stats = compute_statistics(&trades, 2, 0, dec!(100), false);
        assert_eq!(stats.sortino_like, Decimal::ZERO);
    }

    #[test]
    fn sortino_like_nonzero_with_negative_returns() {
        // Returns: -5%, 10% → mean = 2.5, downside_var = 25/2 = 12.5,
        // downside_dev = sqrt(12.5) ≈ 3.5355, sortino = 2.5 / 3.5355 ≈ 0.7071
        let trades = vec![
            make_trade(dec!(-0.5), dec!(-5), 30, false, false),
            make_trade(dec!(1), dec!(10), 30, false, false),
        ];
        let stats = compute_statistics(&trades, 2, 0, dec!(100), false);
        assert!(stats.sortino_like > Decimal::ZERO);
        // Hand-checkable: mean=2.5, downside_dev=sqrt(12.5)≈3.5355
        let expected_sortino = dec!(2.5) / decimal_sqrt(&dec!(12.5));
        assert!((stats.sortino_like - expected_sortino).abs() < dec!(0.001));
    }

    #[test]
    fn oos_verdict_negative_expectancy() {
        // 6 losses, 1 win → mean negative → CI entirely below 0.
        let mut trades = Vec::new();
        for _ in 0..6 {
            trades.push(make_trade(dec!(-1), dec!(-10), 30, false, false));
        }
        trades.push(make_trade(dec!(0.5), dec!(5), 30, false, false));
        let stats = compute_statistics(&trades, 7, 0, dec!(100), false);
        let (ci_lo, ci_hi) = ci95(stats.expectancy_per_trade_pct, stats.standard_error);
        let mut s = stats.clone();
        s.oos_total_trades = 7;
        s.oos_usable_trades = 7;
        s.oos_ambiguous_trades = 0;
        s.oos_censored_trades = 0;
        s.oos_ci95_lower_pct = ci_lo;
        s.oos_ci95_upper_pct = ci_hi;
        assert!(s.expectancy_per_trade_pct < Decimal::ZERO);
        assert!(s.oos_ci95_upper_pct < Decimal::ZERO);
        assert_eq!(compute_oos_verdict(&s), OosVerdict::NegativeExpectancy);
    }

    #[test]
    fn profit_factor_no_losses_returns_max() {
        // All wins, no losses → profit_factor = i32::MAX.
        let trades = vec![
            make_trade(dec!(1), dec!(10), 30, false, false),
            make_trade(dec!(2), dec!(20), 30, false, false),
        ];
        let stats = compute_statistics(&trades, 2, 0, dec!(100), false);
        assert_eq!(stats.profit_factor, Decimal::from(i32::MAX));
    }

    #[test]
    fn profit_factor_no_wins_returns_zero() {
        // All losses, no wins → profit_factor = 0.
        let trades = vec![
            make_trade(dec!(-1), dec!(-10), 30, false, false),
            make_trade(dec!(-2), dec!(-20), 30, false, false),
        ];
        let stats = compute_statistics(&trades, 2, 0, dec!(100), false);
        assert_eq!(stats.profit_factor, Decimal::ZERO);
    }

    #[test]
    fn profit_factor_balanced() {
        // 2 wins (+2, +1), 1 loss (-1) → gross_wins=3, gross_losses=1, pf=3.
        let trades = vec![
            make_trade(dec!(2), dec!(20), 30, false, false),
            make_trade(dec!(1), dec!(10), 30, false, false),
            make_trade(dec!(-1), dec!(-10), 30, false, false),
        ];
        let stats = compute_statistics(&trades, 3, 0, dec!(100), false);
        assert_eq!(stats.profit_factor, dec!(3));
    }

    #[test]
    fn drawdown_zero_when_monotonic_up() {
        // All wins → equity never drops → max_drawdown = 0.
        let trades = vec![
            make_trade(dec!(1), dec!(10), 30, false, false),
            make_trade(dec!(2), dec!(20), 30, false, false),
            make_trade(dec!(1), dec!(10), 30, false, false),
        ];
        let stats = compute_statistics(&trades, 3, 0, dec!(100), false);
        assert_eq!(stats.max_drawdown_pct, Decimal::ZERO);
    }

    #[test]
    fn ambiguous_and_censored_counted_separately() {
        let trades = vec![
            make_trade(dec!(1), dec!(10), 30, false, false),
            make_trade(dec!(-1), dec!(-10), 30, false, false),
            make_trade(dec!(0), dec!(0), 30, true, false),
            make_trade(dec!(0), dec!(0), 30, false, true),
            make_trade(dec!(0), dec!(0), 30, true, true),
        ];
        let stats = compute_statistics(&trades, 5, 0, dec!(100), false);
        assert_eq!(stats.ambiguous_trades, 2); // one ambiguous-only + one both
        assert_eq!(stats.censored_trades, 2); // one censored-only + one both
        assert_eq!(stats.accepted_trades, 5);
        // Only 2 usable trades for win/loss stats.
        assert_eq!(stats.win_rate, dec!(50));
    }

    /// Helper: populate the OOS sample fields on a `BacktestStatistics`
    /// (which the pipeline sets after `compute_statistics`) and return
    /// the verdict. Lets unit tests exercise the verdict logic without
    /// running the full pipeline.
    fn with_oos_fields(
        base: &BacktestStatistics,
        total: usize,
        usable: usize,
        ambiguous: usize,
        censored: usize,
    ) -> OosVerdict {
        let mut s = base.clone();
        s.oos_total_trades = total;
        s.oos_usable_trades = usable;
        s.oos_ambiguous_trades = ambiguous;
        s.oos_censored_trades = censored;
        // For tests that don't care about the CI: the verdict
        // short-circuits on these counts before reaching the CI check,
        // so leaving the CI fields at 0 is safe.
        compute_oos_verdict(&s)
    }
}
