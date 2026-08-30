use crate::{config::types::RiskConfig, economics::{CostModel, EconomicGate, EconomicGateDecision, ViabilityError}};
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

#[derive(Debug, Clone, Default)]
pub struct RiskState { pub equity_usd: Decimal, pub day_start_equity_usd: Decimal, pub total_exposure_usd: Decimal, pub open_positions: usize, pub trades_today: u32, pub cooldown_until: Option<chrono::DateTime<chrono::Utc>> }
pub struct RiskEngine { pub config: RiskConfig, pub kill_switch: KillSwitch, pub state: RiskState }
impl RiskEngine { pub fn new(config: RiskConfig, equity: Decimal) -> Self { let kill_switch=KillSwitch::new(equity,config.max_total_drawdown_before_kill_switch_pct);Self{config,state:RiskState{equity_usd:equity,day_start_equity_usd:equity,..Default::default()},kill_switch} } pub fn authorize(&self, position_usd:Decimal, liquidity_usd:Decimal, expected_slippage_bps:u32, now:chrono::DateTime<chrono::Utc>)->Result<(),RiskError>{if self.kill_switch.is_tripped(){return Err(RiskError::Rejected("kill switch latched".into()))}if self.state.open_positions>=self.config.max_concurrent_positions{return Err(RiskError::Rejected("concurrent-position limit".into()))}if self.state.trades_today>=self.config.max_trades_per_day{return Err(RiskError::Rejected("daily trade-count limit".into()))}if position_usd>self.state.equity_usd*self.config.max_position_percent_of_equity/dec!(100)||position_usd>liquidity_usd*self.config.max_position_percent_of_liquidity/dec!(100){return Err(RiskError::Rejected("position exceeds equity/liquidity limit".into()))}if expected_slippage_bps>self.config.max_slippage_bps{return Err(RiskError::Rejected("slippage limit".into()))}if let Some(until)=self.state.cooldown_until {if now<until{return Err(RiskError::Rejected("loss cooldown active".into()))}}let loss_pct=(self.state.day_start_equity_usd-self.state.equity_usd)/self.state.day_start_equity_usd*dec!(100);if loss_pct>=self.config.max_daily_loss_percent{return Err(RiskError::Rejected("daily-loss limit".into()))}Ok(())} pub fn observe_equity(&mut self,equity:Decimal){self.state.equity_usd=equity;self.kill_switch.observe_equity(equity)} }

#[cfg(test)]
mod tests { use super::*; use rust_decimal_macros::dec;
    #[test] fn kill_switch_latches_when_equity_crosses_threshold_mid_position() { let mut k = KillSwitch::new(dec!(20), dec!(50)); k.observe_equity(dec!(10.01)); assert!(!k.is_tripped()); k.observe_equity(dec!(10)); assert!(k.is_tripped()); k.observe_equity(dec!(19)); assert!(k.is_tripped()); }
}
