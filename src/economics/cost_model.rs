use super::{break_even_calculator, BreakEvenInputs, BreakEvenResult, ViabilityError};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

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
