use crate::backtest::engine::SimulatedTrade;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestStatistics {
    pub total_signals: usize,
    pub accepted_trades: usize,
    pub rejected_signals: usize,
    pub ambiguous_trades: usize,
    pub win_rate: Decimal,
    pub avg_win_pct: Decimal,
    pub avg_loss_pct: Decimal,
    pub median_return_pct: Decimal,
    pub expectancy_per_trade_pct: Decimal,
    pub profit_factor: Decimal,
    pub gross_pnl_usd: Decimal,
    pub net_pnl_usd: Decimal,
    pub total_costs_usd: Decimal,
    pub max_drawdown_pct: Decimal,
    pub longest_losing_streak: usize,
    pub avg_holding_minutes: Decimal,
    pub return_std_dev: Decimal,
    pub standard_error: Decimal,
    pub t_statistic: Decimal,
    pub sharpe_like: Decimal,
    pub sortino_like: Decimal,
}

impl fmt::Display for BacktestStatistics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "=== Backtest Statistics ===")?;
        writeln!(
            f,
            "Signals:            {} total, {} accepted, {} rejected",
            self.total_signals, self.accepted_trades, self.rejected_signals
        )?;
        writeln!(
            f,
            "Ambiguous trades:   {} (excluded from win/loss stats)",
            self.ambiguous_trades
        )?;
        writeln!(f)?;
        writeln!(f, "Win rate:           {}", self.win_rate)?;
        writeln!(f, "Avg win:            {}%", self.avg_win_pct)?;
        writeln!(f, "Avg loss:           {}%", self.avg_loss_pct)?;
        writeln!(f, "Median return:      {}%", self.median_return_pct)?;
        writeln!(f, "Expectancy/trade:   {}%", self.expectancy_per_trade_pct)?;
        writeln!(f, "Profit factor:      {}", self.profit_factor)?;
        writeln!(f)?;
        writeln!(f, "Gross PnL:          ${}", self.gross_pnl_usd)?;
        writeln!(f, "Net PnL:            ${}", self.net_pnl_usd)?;
        writeln!(f, "Total costs:        ${}", self.total_costs_usd)?;
        writeln!(f, "Max drawdown:       {}%", self.max_drawdown_pct)?;
        writeln!(
            f,
            "Losing streak:      {} trades",
            self.longest_losing_streak
        )?;
        writeln!(f, "Avg holding:        {} min", self.avg_holding_minutes)?;
        writeln!(f)?;
        writeln!(f, "Return std dev:     {}%", self.return_std_dev)?;
        writeln!(f, "Standard error:     {}%", self.standard_error)?;
        writeln!(f, "t-statistic:        {}", self.t_statistic)?;
        writeln!(f, "Sharpe-like:        {}", self.sharpe_like)?;
        writeln!(f, "Sortino-like:       {}", self.sortino_like)?;
        Ok(())
    }
}

pub fn compute_statistics(
    trades: &[SimulatedTrade],
    total_signals: usize,
    rejected_count: usize,
) -> BacktestStatistics {
    let accepted = trades.len();
    let ambiguous = trades.iter().filter(|t| t.is_ambiguous).count();
    let non_ambiguous: Vec<&SimulatedTrade> = trades.iter().filter(|t| !t.is_ambiguous).collect();

    let wins: Vec<&&SimulatedTrade> = non_ambiguous
        .iter()
        .filter(|t| t.net_pnl_usd > Decimal::ZERO)
        .collect();
    let losses: Vec<&&SimulatedTrade> = non_ambiguous
        .iter()
        .filter(|t| t.net_pnl_usd <= Decimal::ZERO)
        .collect();

    let n = Decimal::from(non_ambiguous.len());
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

    let mut returns: Vec<Decimal> = non_ambiguous.iter().map(|t| t.net_return_pct).collect();
    let median_return_pct = median(&mut returns);

    let expectancy = if n > Decimal::ZERO {
        non_ambiguous
            .iter()
            .map(|t| t.net_return_pct)
            .sum::<Decimal>()
            / n
    } else {
        Decimal::ZERO
    };

    let gross_wins: Decimal = wins.iter().map(|t| t.net_pnl_usd).sum();
    let gross_losses: Decimal = losses.iter().map(|t| t.net_pnl_usd.abs()).sum();
    let profit_factor = if gross_losses > Decimal::ZERO {
        gross_wins / gross_losses
    } else if gross_wins > Decimal::ZERO {
        Decimal::from(i32::MAX)
    } else {
        Decimal::ZERO
    };

    let gross_pnl: Decimal = trades.iter().map(|t| t.gross_pnl_usd).sum();
    let net_pnl: Decimal = trades.iter().map(|t| t.net_pnl_usd).sum();
    let total_costs: Decimal = trades.iter().map(|t| t.total_cost_usd).sum();

    let mut cumulative = Decimal::ZERO;
    let mut peak = Decimal::ZERO;
    let mut max_dd = Decimal::ZERO;
    for t in non_ambiguous.iter() {
        cumulative += t.net_pnl_usd;
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

    let mut longest_streak = 0usize;
    let mut current_streak = 0usize;
    for t in non_ambiguous.iter() {
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
        non_ambiguous
            .iter()
            .map(|t| t.net_return_pct)
            .sum::<Decimal>()
            / n
    } else {
        Decimal::ZERO
    };
    let variance = if n > Decimal::ONE {
        non_ambiguous
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
        non_ambiguous
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

    BacktestStatistics {
        total_signals,
        accepted_trades: accepted,
        rejected_signals: rejected_count,
        ambiguous_trades: ambiguous,
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
            exit_reason: ExitReason::TakeProfit,
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
        }
    }

    #[test]
    fn empty_trades() {
        let stats = compute_statistics(&[], 10, 5);
        assert_eq!(stats.total_signals, 10);
        assert_eq!(stats.accepted_trades, 0);
        assert_eq!(stats.rejected_signals, 5);
        assert_eq!(stats.win_rate, Decimal::ZERO);
    }

    #[test]
    fn win_rate_computed_correctly() {
        let trades = vec![
            make_trade(dec!(1), dec!(10), 30, false),
            make_trade(dec!(1), dec!(10), 30, false),
            make_trade(dec!(-1), dec!(-10), 30, false),
            make_trade(dec!(-1), dec!(-10), 30, false),
        ];
        let stats = compute_statistics(&trades, 4, 0);
        assert_eq!(stats.win_rate, dec!(50));
    }

    #[test]
    fn ambiguous_trades_excluded_from_win_rate() {
        let trades = vec![
            make_trade(dec!(1), dec!(10), 30, false),
            make_trade(dec!(1), dec!(10), 30, false),
            make_trade(dec!(-1), dec!(-10), 30, true),
        ];
        let stats = compute_statistics(&trades, 3, 0);
        assert_eq!(stats.ambiguous_trades, 1);
        assert_eq!(stats.win_rate, dec!(100));
    }

    #[test]
    fn max_drawdown_positive() {
        let trades = vec![
            make_trade(dec!(2), dec!(20), 30, false),
            make_trade(dec!(-3), dec!(-30), 30, false),
            make_trade(dec!(1), dec!(10), 30, false),
        ];
        let stats = compute_statistics(&trades, 3, 0);
        assert!(stats.max_drawdown_pct > Decimal::ZERO);
    }

    #[test]
    fn longest_losing_streak_counted() {
        let trades = vec![
            make_trade(dec!(1), dec!(10), 30, false),
            make_trade(dec!(-1), dec!(-10), 30, false),
            make_trade(dec!(-1), dec!(-10), 30, false),
            make_trade(dec!(-1), dec!(-10), 30, false),
            make_trade(dec!(1), dec!(10), 30, false),
        ];
        let stats = compute_statistics(&trades, 5, 0);
        assert_eq!(stats.longest_losing_streak, 3);
    }

    #[test]
    fn profit_factor_calculation() {
        let trades = vec![
            make_trade(dec!(2), dec!(20), 30, false),
            make_trade(dec!(1), dec!(10), 30, false),
            make_trade(dec!(-1), dec!(-10), 30, false),
        ];
        let stats = compute_statistics(&trades, 3, 0);
        assert_eq!(stats.profit_factor, dec!(3));
    }

    #[test]
    fn total_costs_summed() {
        let trades = vec![make_trade(dec!(0), dec!(0), 30, false)];
        let stats = compute_statistics(&trades, 1, 0);
        assert_eq!(stats.total_costs_usd, dec!(0.088));
    }

    #[test]
    fn holding_time_averaged() {
        let trades = vec![
            make_trade(dec!(1), dec!(10), 30, false),
            make_trade(dec!(1), dec!(10), 60, false),
        ];
        let stats = compute_statistics(&trades, 2, 0);
        assert_eq!(stats.avg_holding_minutes, dec!(45));
    }

    #[test]
    fn t_statistic_nonzero_with_data() {
        let trades = vec![
            make_trade(dec!(1), dec!(10), 30, false),
            make_trade(dec!(2), dec!(20), 30, false),
            make_trade(dec!(-1), dec!(-10), 30, false),
        ];
        let stats = compute_statistics(&trades, 3, 0);
        assert!(stats.t_statistic > Decimal::ZERO);
    }

    #[test]
    fn sharpe_like_zero_when_no_variance() {
        let trades = vec![
            make_trade(dec!(1), dec!(10), 30, false),
            make_trade(dec!(1), dec!(10), 30, false),
        ];
        let stats = compute_statistics(&trades, 2, 0);
        assert_eq!(stats.sharpe_like, Decimal::ZERO);
    }
}
