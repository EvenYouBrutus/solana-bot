use crate::domain::trade::OrderState;
use crate::storage::StateStore;
use anyhow::{Context, Result};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

/// Comprehensive performance report from persisted trade data.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PerformanceReport {
    pub total_trades: u32,
    pub wins: u32,
    pub losses: u32,
    pub total_pnl_usd: Decimal,
    pub total_fees_usd: Decimal,
    pub win_rate: Decimal,
    pub expectancy: Decimal,
    pub profit_factor: Decimal,
    pub max_drawdown_usd: Decimal,
    pub max_drawdown_pct: Decimal,
    pub peak_equity_usd: Decimal,
    pub min_equity_usd: Decimal,
    pub avg_holding_minutes: Decimal,
    pub exits_by_reason: HashMap<String, u32>,
    pub rejections_by_reason: HashMap<String, u32>,
    pub starting_capital_usd: Decimal,
    pub ending_equity_usd: Decimal,
}

impl PerformanceReport {
    pub fn new(starting_capital_usd: Decimal) -> Self {
        Self {
            starting_capital_usd,
            peak_equity_usd: starting_capital_usd,
            min_equity_usd: starting_capital_usd,
            ..Default::default()
        }
    }

    /// Generate a performance report from persisted store data.
    pub fn generate(store: &StateStore, starting_capital: Decimal) -> Result<Self> {
        let mut report = PerformanceReport::new(starting_capital);
        let orders = store.orders().with_context(|| "load orders")?;
        let positions = store.positions().with_context(|| "load positions")?;

        for pos in &positions {
            let _entry_cost = pos.entry_cost_usd.unwrap_or(Decimal::ZERO);
            if pos.state == crate::domain::position::PositionState::Closed {
                let pnl = pos.realized_pnl_usd;
                let fees = pos.fees_usd + pos.exit_fees_usd.unwrap_or(Decimal::ZERO);
                let exit_reason = pos.exit_reason.clone().unwrap_or_default();
                let is_win = pnl > Decimal::ZERO;
                let _hold_minutes = pos
                    .exit_time
                    .map(|et| (et - pos.entry_time).num_minutes() as f64)
                    .unwrap_or(0.0);

                report.total_trades += 1;
                report.total_pnl_usd += pnl;
                report.total_fees_usd += fees;
                if is_win {
                    report.wins += 1;
                } else {
                    report.losses += 1;
                }
                *report.exits_by_reason.entry(exit_reason).or_insert(0) += 1;

                // Track equity.
                let current_equity = report.peak_equity_usd + pnl;
                if current_equity > report.peak_equity_usd {
                    report.peak_equity_usd = current_equity;
                }
                if current_equity < report.min_equity_usd {
                    report.min_equity_usd = current_equity;
                }
                report.ending_equity_usd = current_equity;
            }
        }

        // Compute metrics.
        if report.total_trades > 0 {
            report.win_rate =
                Decimal::from(report.wins) / Decimal::from(report.total_trades) * dec!(100);
            report.expectancy = report.total_pnl_usd / Decimal::from(report.total_trades);
        }

        let total_wins_pnl: Decimal = report.total_pnl_usd.max(Decimal::ZERO);
        let total_losses_pnl: Decimal = report.total_pnl_usd.abs();
        report.profit_factor = if total_losses_pnl > Decimal::ZERO {
            total_wins_pnl / total_losses_pnl
        } else if total_wins_pnl > Decimal::ZERO {
            Decimal::from(u64::MAX)
        } else {
            Decimal::ZERO
        };

        // Drawdown.
        if report.peak_equity_usd > Decimal::ZERO {
            let dd = report.peak_equity_usd - report.min_equity_usd;
            report.max_drawdown_usd = dd;
            report.max_drawdown_pct = dd / report.peak_equity_usd * dec!(100);
        }

        // Count failed/rejected orders.
        for order in &orders {
            if order.state == OrderState::Failed {
                let reason = order.error.as_deref().unwrap_or("unknown").to_string();
                *report.rejections_by_reason.entry(reason).or_insert(0) += 1;
            }
        }

        Ok(report)
    }

    pub fn summary(&self) -> String {
        format!(
            "Trades: {} | Wins: {} ({:.1}%) | PnL: ${:.4} | Fees: ${:.4} | PF: {:.2} | MaxDD: ${:.4} ({:.1}%) | Expectancy: ${:.4}",
            self.total_trades,
            self.wins,
            self.win_rate,
            self.total_pnl_usd,
            self.total_fees_usd,
            self.profit_factor,
            self.max_drawdown_usd,
            self.max_drawdown_pct,
            self.expectancy,
        )
    }
}

impl fmt::Display for PerformanceReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "=== PERFORMANCE REPORT ===")?;
        writeln!(f)?;
        writeln!(f, "--- Trade Summary ---")?;
        writeln!(f, "  Total Trades:  {}", self.total_trades)?;
        writeln!(f, "  Wins:          {}", self.wins)?;
        writeln!(f, "  Losses:        {}", self.losses)?;
        writeln!(f, "  Win Rate:      {:.1}%", self.win_rate)?;
        writeln!(f, "  Avg Hold:      {:.0} min", self.avg_holding_minutes)?;
        writeln!(f)?;
        writeln!(f, "--- P&L ---")?;
        writeln!(f, "  Total PnL:     ${:.4}", self.total_pnl_usd)?;
        writeln!(f, "  Total Fees:    ${:.4}", self.total_fees_usd)?;
        writeln!(f, "  Expectancy:    ${:.4}", self.expectancy)?;
        writeln!(f, "  Profit Factor: {:.2}", self.profit_factor)?;
        writeln!(f)?;
        writeln!(f, "--- Risk ---")?;
        writeln!(f, "  Starting:      ${:.2}", self.starting_capital_usd)?;
        writeln!(f, "  Ending:        ${:.4}", self.ending_equity_usd)?;
        writeln!(f, "  Peak:          ${:.4}", self.peak_equity_usd)?;
        writeln!(
            f,
            "  Max DD:        ${:.4} ({:.1}%)",
            self.max_drawdown_usd, self.max_drawdown_pct
        )?;
        writeln!(f)?;
        writeln!(f, "--- Exit Breakdown ---")?;
        for (reason, count) in &self.exits_by_reason {
            writeln!(f, "  {:<25} {}", reason, count)?;
        }
        if self.exits_by_reason.is_empty() {
            writeln!(f, "  (no exits recorded)")?;
        }
        writeln!(f)?;
        writeln!(f, "--- Rejection Breakdown ---")?;
        for (reason, count) in &self.rejections_by_reason {
            writeln!(f, "  {:<25} {}", reason, count)?;
        }
        if self.rejections_by_reason.is_empty() {
            writeln!(f, "  (no rejections recorded)")?;
        }
        writeln!(f, "========================")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn new_report_starts_with_zero() {
        let report = PerformanceReport::new(dec!(100));
        assert_eq!(report.total_trades, 0);
        assert_eq!(report.starting_capital_usd, dec!(100));
        assert_eq!(report.peak_equity_usd, dec!(100));
    }

    #[test]
    fn empty_report_generates() {
        let store = StateStore::open(":memory:").unwrap();
        let report = PerformanceReport::generate(&store, dec!(100)).unwrap();
        assert_eq!(report.total_trades, 0);
        assert_eq!(report.win_rate, dec!(0));
        assert_eq!(report.profit_factor, dec!(0));
        assert_eq!(report.starting_capital_usd, dec!(100));
    }

    #[test]
    fn report_with_closed_position() {
        use crate::domain::position::{Position, PositionState, ReconciliationStatus};
        use chrono::Utc;

        let store = StateStore::open(":memory:").unwrap();
        let pos = Position {
            mint: "T1".into(),
            position_id: Some("p1".into()),
            token_mint: Some("T1".into()),
            base_mint: Some("SOL".into()),
            entry_input_amount_atomic: Some(1_000_000),
            entry_output_amount_atomic: Some(5_000_000),
            token_decimals: Some(6),
            base_mint_decimals: Some(9),
            entry_fees_usd: Some(dec!(0.01)),
            entry_slippage_bps: Some(30),
            entry_cost_model: None,
            quantity: dec!(5_000_000),
            remaining_quantity_atomic: Some(0),
            entry_cost_usd: Some(dec!(10)),
            base_entry_price_usd: Some(dec!(150)),
            state: PositionState::Closed,
            reconciliation_status: ReconciliationStatus::Reconciled,
            last_reconciled_at: None,
            exit_signature: Some("paper:exit1".into()),
            exit_fees_usd: Some(dec!(0.01)),
            exit_time: Some(Utc::now()),
            entry_price_usd: dec!(0.002),
            entry_time: Utc::now() - chrono::Duration::hours(2),
            entry_signature: "paper:entry1".into(),
            high_water_price_usd: dec!(0.003),
            realized_pnl_usd: dec!(5),
            unrealized_pnl_usd: Decimal::ZERO,
            fees_usd: dec!(0.02),
            current_value_usd: dec!(15),
            signal_id: "s1".into(),
            exit_reason: Some("take_profit".into()),
        };
        store.save_position(&pos).unwrap();

        let report = PerformanceReport::generate(&store, dec!(100)).unwrap();
        assert_eq!(report.total_trades, 1);
        assert_eq!(report.wins, 1);
        assert_eq!(report.total_pnl_usd, dec!(5));
        assert_eq!(report.exits_by_reason["take_profit"], 1);
    }

    #[test]
    fn summary_format() {
        let mut report = PerformanceReport::new(dec!(100));
        report.total_trades = 5;
        report.wins = 3;
        report.losses = 2;
        report.win_rate = dec!(60);
        report.total_pnl_usd = dec!(2.50);
        report.profit_factor = dec!(1.5);
        let summary = report.summary();
        assert!(summary.contains("Trades: 5"));
        assert!(summary.contains("Wins: 3"));
    }

    #[test]
    fn display_format() {
        let report = PerformanceReport::new(dec!(100));
        let display = format!("{report}");
        assert!(display.contains("PERFORMANCE REPORT"));
        assert!(display.contains("Trade Summary"));
        assert!(display.contains("P&L"));
        assert!(display.contains("Risk"));
        assert!(display.contains("Exit Breakdown"));
        assert!(display.contains("Rejection Breakdown"));
    }
}
