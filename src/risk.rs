use crate::economics::{CostModel, EconomicGate, EconomicGateDecision, ViabilityError};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RiskError { #[error("economic viability check failed: {0}")] Economics(#[from] ViabilityError), #[error("entry rejected: {0}")] Rejected(String) }

/// Stateful and intentionally monotonic during a process lifetime. A config
/// reload cannot clear this latch; only an explicit operator recovery workflow
/// may construct a new live session after review.
#[derive(Debug, Clone)]
pub struct KillSwitch { starting_capital_usd: Decimal, max_drawdown_pct: Decimal, tripped: bool }
impl KillSwitch {
    pub fn new(starting_capital_usd: Decimal, max_drawdown_pct: Decimal) -> Self { Self { starting_capital_usd, max_drawdown_pct, tripped: false } }
    pub fn observe_equity(&mut self, equity_usd: Decimal) { if equity_usd <= self.starting_capital_usd * (Decimal::ONE - self.max_drawdown_pct / dec!(100)) { self.tripped = true; } }
    pub fn is_tripped(&self) -> bool { self.tripped }
}

pub fn authorize_entry(kill_switch: &KillSwitch, gate: &EconomicGate, model: &CostModel) -> Result<(), RiskError> {
    if kill_switch.is_tripped() { return Err(RiskError::Rejected("kill switch is latched; no new entries".into())); }
    match gate.check(model)? { EconomicGateDecision::Allowed(_) => Ok(()), EconomicGateDecision::Rejected { result, threshold_pct } => Err(RiskError::Rejected(format!("round-trip cost {}% exceeds {}% threshold", result.round_trip_cost_pct_of_position, threshold_pct))) }
}

#[cfg(test)]
mod tests { use super::*; use rust_decimal_macros::dec;
    #[test] fn kill_switch_latches_when_equity_crosses_threshold_mid_position() { let mut k = KillSwitch::new(dec!(20), dec!(50)); k.observe_equity(dec!(10.01)); assert!(!k.is_tripped()); k.observe_equity(dec!(10)); assert!(k.is_tripped()); k.observe_equity(dec!(19)); assert!(k.is_tripped()); }
}
