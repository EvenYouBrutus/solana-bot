use super::{break_even_calculator, BreakEvenInputs, BreakEvenResult, ViabilityError};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use rust_decimal_macros::dec;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpectedValue {
    pub gross_return_pct: Decimal,
    pub trading_fees_pct: Decimal,
    pub priority_and_network_pct: Decimal,
    pub slippage_pct: Decimal,
    pub price_impact_pct: Decimal,
    pub failed_transaction_pct: Decimal,
    pub adverse_selection_pct: Decimal,
    pub expected_exit_cost_pct: Decimal,
    pub uncertainty_haircut_pct: Decimal,
    pub net_return_pct: Decimal,
}
impl ExpectedValue {
    pub fn estimate(gross_return_pct: Decimal, model: &CostModel, adverse_selection_bps: Decimal, exit_cost_bps: Decimal, uncertainty_haircut_pct: Decimal) -> Result<Self, ViabilityError> {
        let r = model.calculate()?;
        if adverse_selection_bps < Decimal::ZERO || exit_cost_bps < Decimal::ZERO || uncertainty_haircut_pct < Decimal::ZERO { return Err(ViabilityError::Negative("expected-value component")); }
        let p = model.input.position_size_usd;
        let fees = dec!(2) * model.input.avg_swap_fee_bps / dec!(100);
        let priority = dec!(2) * model.input.avg_priority_fee_usd / p * dec!(100);
        let slippage = dec!(2) * model.input.avg_slippage_bps / dec!(100);
        let impact = dec!(2) * model.input.avg_price_impact_bps / dec!(100);
        let failed = r.expected_failed_tx_cost_usd / p * dec!(100);
        let adverse = adverse_selection_bps / dec!(100);
        let exit = exit_cost_bps / dec!(100);
        let net = gross_return_pct - fees - priority - slippage - impact - failed - adverse - exit - uncertainty_haircut_pct;
        Ok(Self { gross_return_pct, trading_fees_pct: fees, priority_and_network_pct: priority, slippage_pct: slippage, price_impact_pct: impact, failed_transaction_pct: failed, adverse_selection_pct: adverse, expected_exit_cost_pct: exit, uncertainty_haircut_pct, net_return_pct: net })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostModel {
    pub observed_at: DateTime<Utc>,
    pub input: BreakEvenInputs,
    pub source: String,
    pub is_live_snapshot: bool,
}
impl CostModel { pub fn calculate(&self) -> Result<BreakEvenResult, ViabilityError> { break_even_calculator(&self.input) } }

#[derive(Debug, Clone, PartialEq)]
pub enum EconomicGateDecision { Allowed(BreakEvenResult), Rejected { result: BreakEvenResult, threshold_pct: Decimal } }

#[derive(Debug, Clone)]
pub struct EconomicGate { pub round_trip_cost_threshold_pct: Decimal }
impl EconomicGate {
    /// Fail closed: callers must not convert a calculation error into permission.
    pub fn check(&self, model: &CostModel) -> Result<EconomicGateDecision, ViabilityError> {
        let result = model.calculate()?;
        if result.round_trip_cost_pct_of_position > self.round_trip_cost_threshold_pct { Ok(EconomicGateDecision::Rejected { result, threshold_pct: self.round_trip_cost_threshold_pct }) } else { Ok(EconomicGateDecision::Allowed(result)) }
    }
}

#[cfg(test)]
mod tests { use super::*; use rust_decimal_macros::dec; use chrono::Utc;
    #[test] fn rejects_cost_dominated_trade() { let model = CostModel { observed_at: Utc::now(), source: "test".into(), is_live_snapshot: false, input: BreakEvenInputs { position_size_usd: dec!(3), avg_priority_fee_usd: dec!(0.01), avg_swap_fee_bps: dec!(30), avg_slippage_bps: dec!(300), avg_price_impact_bps: dec!(100), failed_tx_rate: dec!(0.2), avg_failed_tx_cost_usd: dec!(0.01), assumed_win_loss_ratio: dec!(2), assumed_avg_loss_pct: dec!(10) } }; assert!(matches!(EconomicGate { round_trip_cost_threshold_pct: dec!(8) }.check(&model), Ok(EconomicGateDecision::Rejected { .. }))); }
}
