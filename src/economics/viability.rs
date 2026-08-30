use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BreakEvenInputs {
    pub position_size_usd: Decimal,
    /// Network fee per submitted swap attempt in USD. This MUST include the
    /// 5,000-lamport base fee plus the selected priority fee.
    pub avg_priority_fee_usd: Decimal,
    /// Per-fill AMM/aggregator fee, not a user slippage tolerance.
    pub avg_swap_fee_bps: Decimal,
    /// Expected realised adverse execution per fill, calibrated from fills.
    pub avg_slippage_bps: Decimal,
    pub avg_price_impact_bps: Decimal,
    /// Probability that one submitted swap attempt fails and still costs a fee.
    pub failed_tx_rate: Decimal,
    pub avg_failed_tx_cost_usd: Decimal,
    /// Gross average winner divided by gross average loser. Required because a
    /// break-even win rate is undefined without an assumed payoff ratio.
    pub assumed_win_loss_ratio: Decimal,
    /// Gross average losing move, as a percent of position (for required win %).
    pub assumed_avg_loss_pct: Decimal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BreakEvenResult {
    pub round_trip_cost_usd: Decimal,
    pub round_trip_cost_pct_of_position: Decimal,
    pub required_win_rate_at_given_rr: Decimal,
    pub required_avg_win_pct: Decimal,
    pub expected_failed_tx_cost_usd: Decimal,
}

#[derive(Debug, Error, PartialEq)]
pub enum ViabilityError {
    #[error("{0} must be greater than zero")]
    NonPositive(&'static str),
    #[error("{0} must be between 0 and 1")]
    ProbabilityOutOfRange(&'static str),
    #[error("cost inputs cannot be negative: {0}")]
    Negative(&'static str),
}

/// Calculates expected round-trip costs before any directional P&L. The failed
/// transaction component is `2 * failed_tx_rate * avg_failed_tx_cost_usd`: an
/// expected failed attempt on either entry or exit, in addition to successful
/// attempts' network fees.
pub fn break_even_calculator(input: &BreakEvenInputs) -> Result<BreakEvenResult, ViabilityError> {
    positive(input.position_size_usd, "position_size_usd")?;
    positive(input.assumed_win_loss_ratio, "assumed_win_loss_ratio")?;
    positive(input.assumed_avg_loss_pct, "assumed_avg_loss_pct")?;
    probability(input.failed_tx_rate, "failed_tx_rate")?;
    for (value, name) in [
        (input.avg_priority_fee_usd, "avg_priority_fee_usd"),
        (input.avg_swap_fee_bps, "avg_swap_fee_bps"),
        (input.avg_slippage_bps, "avg_slippage_bps"),
        (input.avg_price_impact_bps, "avg_price_impact_bps"),
        (input.avg_failed_tx_cost_usd, "avg_failed_tx_cost_usd"),
    ] { if value < Decimal::ZERO { return Err(ViabilityError::Negative(name)); } }

    let per_fill_bps = input.avg_swap_fee_bps + input.avg_slippage_bps + input.avg_price_impact_bps;
    let fill_cost = input.position_size_usd * per_fill_bps / dec!(10000);
    let expected_failed_tx_cost_usd = dec!(2) * input.failed_tx_rate * input.avg_failed_tx_cost_usd;
    let round_trip_cost_usd = dec!(2) * (fill_cost + input.avg_priority_fee_usd) + expected_failed_tx_cost_usd;
    let cost_pct = round_trip_cost_usd / input.position_size_usd * dec!(100);

    // With a gross loss of L and gross win R*L, net outcomes are win=R*L-C
    // and loss=-L-C. Solve p(win)+(1-p)(loss)=0 for p.
    let loss = input.assumed_avg_loss_pct;
    let required_win_rate = (loss + cost_pct) / (loss * (Decimal::ONE + input.assumed_win_loss_ratio));
    let required_avg_win_pct = (loss + cost_pct) / input.assumed_win_loss_ratio + cost_pct;
    Ok(BreakEvenResult { round_trip_cost_usd, round_trip_cost_pct_of_position: cost_pct, required_win_rate_at_given_rr: required_win_rate, required_avg_win_pct, expected_failed_tx_cost_usd })
}

fn positive(value: Decimal, name: &'static str) -> Result<(), ViabilityError> { if value > Decimal::ZERO { Ok(()) } else { Err(ViabilityError::NonPositive(name)) } }
fn probability(value: Decimal, name: &'static str) -> Result<(), ViabilityError> { if value >= Decimal::ZERO && value <= Decimal::ONE { Ok(()) } else { Err(ViabilityError::ProbabilityOutOfRange(name)) } }

#[cfg(test)]
mod tests {
    use super::*;
    fn input() -> BreakEvenInputs { BreakEvenInputs { position_size_usd: dec!(10), avg_priority_fee_usd: dec!(0.002), avg_swap_fee_bps: dec!(30), avg_slippage_bps: dec!(100), avg_price_impact_bps: dec!(20), failed_tx_rate: dec!(0.10), avg_failed_tx_cost_usd: dec!(0.002), assumed_win_loss_ratio: dec!(2), assumed_avg_loss_pct: dec!(10) } }
    #[test] fn includes_all_expected_costs_and_solves_break_even() { let r = break_even_calculator(&input()).unwrap(); assert_eq!(r.round_trip_cost_usd, dec!(0.3044)); assert_eq!(r.round_trip_cost_pct_of_position, dec!(3.04400)); assert_eq!(r.required_win_rate_at_given_rr.round_dp(4), dec!(0.4348)); }
    #[test] fn rejects_adversarial_invalid_probability() { let mut i = input(); i.failed_tx_rate = dec!(1.01); assert_eq!(break_even_calculator(&i).unwrap_err(), ViabilityError::ProbabilityOutOfRange("failed_tx_rate")); }
}
