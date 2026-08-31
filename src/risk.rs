use crate::{
    config::types::RiskConfig,
    economics::{CostModel, EconomicGate, EconomicGateDecision, ViabilityError},
};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RiskError {
    #[error("economic viability check failed: {0}")]
    Economics(#[from] ViabilityError),
    #[error("entry rejected: {0}")]
    Rejected(String),
}

/// Stateful and intentionally monotonic during a process lifetime. A config
/// reload cannot clear this latch; only an explicit operator recovery workflow
/// may construct a new live session after review.
#[derive(Debug, Clone)]
pub struct KillSwitch {
    starting_capital_usd: Decimal,
    max_drawdown_pct: Decimal,
    tripped: bool,
}
impl KillSwitch {
    pub fn new(starting_capital_usd: Decimal, max_drawdown_pct: Decimal) -> Self {
        Self {
            starting_capital_usd,
            max_drawdown_pct,
            tripped: false,
        }
    }
    pub fn observe_equity(&mut self, equity_usd: Decimal) {
        if equity_usd
            <= self.starting_capital_usd * (Decimal::ONE - self.max_drawdown_pct / dec!(100))
        {
            self.tripped = true;
        }
    }
    pub fn force_trip(&mut self) {
        self.tripped = true;
    }
    pub fn is_tripped(&self) -> bool {
        self.tripped
    }
}

pub fn authorize_entry(
    kill_switch: &KillSwitch,
    gate: &EconomicGate,
    model: &CostModel,
) -> Result<(), RiskError> {
    if kill_switch.is_tripped() {
        return Err(RiskError::Rejected(
            "kill switch is latched; no new entries".into(),
        ));
    }
    match gate.check(model)? {
        EconomicGateDecision::Allowed(_) => Ok(()),
        EconomicGateDecision::Rejected {
            result,
            threshold_pct,
        } => Err(RiskError::Rejected(format!(
            "round-trip cost {}% exceeds {}% threshold",
            result.round_trip_cost_pct_of_position, threshold_pct
        ))),
    }
}

#[derive(Debug, Clone, Default)]
pub struct RiskState {
    pub equity_usd: Decimal,
    pub day_start_equity_usd: Decimal,
    pub total_exposure_usd: Decimal,
    pub open_positions: usize,
    pub trades_today: u32,
    pub cooldown_until: Option<DateTime<Utc>>,
}
pub struct RiskEngine {
    pub config: RiskConfig,
    pub kill_switch: KillSwitch,
    pub state: RiskState,
    consecutive_failures: u32,
}

impl RiskEngine {
    pub fn new(config: RiskConfig, equity: Decimal) -> Self {
        let kill_switch = KillSwitch::new(equity, config.max_total_drawdown_before_kill_switch_pct);
        Self {
            config,
            state: RiskState {
                equity_usd: equity,
                day_start_equity_usd: equity,
                ..Default::default()
            },
            kill_switch,
            consecutive_failures: 0,
        }
    }
    /// Every new position must pass here; the execution layer has no path that
    /// bypasses this check. Fail closed on any unmeasurable condition.
    pub fn authorize(
        &self,
        position_usd: Decimal,
        liquidity_usd: Decimal,
        expected_slippage_bps: u32,
        price_impact_bps: u32,
        now: DateTime<Utc>,
    ) -> Result<(), RiskError> {
        if self.kill_switch.is_tripped() {
            return Err(RiskError::Rejected("kill switch latched".into()));
        }
        if position_usd <= Decimal::ZERO {
            return Err(RiskError::Rejected("position size must be positive".into()));
        }
        if liquidity_usd <= Decimal::ZERO {
            return Err(RiskError::Rejected(
                "liquidity unknown; refusing to size a position".into(),
            ));
        }
        if self.state.open_positions >= self.config.max_concurrent_positions {
            return Err(RiskError::Rejected("concurrent-position limit".into()));
        }
        if self.state.trades_today >= self.config.max_trades_per_day {
            return Err(RiskError::Rejected("daily trade-count limit".into()));
        }
        if position_usd
            > self.state.equity_usd * self.config.max_position_percent_of_equity / dec!(100)
            || position_usd
                > liquidity_usd * self.config.max_position_percent_of_liquidity / dec!(100)
        {
            return Err(RiskError::Rejected(
                "position exceeds equity/liquidity limit".into(),
            ));
        }
        if expected_slippage_bps > self.config.max_slippage_bps {
            return Err(RiskError::Rejected("slippage limit".into()));
        }
        if price_impact_bps > self.config.max_slippage_bps {
            return Err(RiskError::Rejected("price-impact limit".into()));
        }
        if let Some(until) = self.state.cooldown_until {
            if now < until {
                return Err(RiskError::Rejected("loss cooldown active".into()));
            }
        }
        if self.state.day_start_equity_usd > Decimal::ZERO {
            let loss_pct = (self.state.day_start_equity_usd - self.state.equity_usd)
                / self.state.day_start_equity_usd
                * dec!(100);
            if loss_pct >= self.config.max_daily_loss_percent {
                return Err(RiskError::Rejected("daily-loss limit".into()));
            }
        }
        Ok(())
    }
    /// Exits reduce risk and are permitted even under kill switch, emergency
    /// stop, cooldown, and daily-loss limits. Only the position's existence
    /// and a positive size are required.
    pub fn authorize_exit(&self, remaining_qty: u64) -> Result<(), RiskError> {
        if remaining_qty == 0 {
            return Err(RiskError::Rejected("nothing to exit".into()));
        }
        Ok(())
    }
    pub fn observe_equity(&mut self, equity: Decimal) {
        self.state.equity_usd = equity;
        self.kill_switch.observe_equity(equity);
    }
    pub fn register_trade(&mut self) {
        self.state.trades_today = self.state.trades_today.saturating_add(1);
    }
    pub fn register_exit(&mut self) {
        self.state.open_positions = self.state.open_positions.saturating_sub(1);
    }
    /// Losses impose a trade cooldown; successes clear the failure streak.
    pub fn record_execution_success(&mut self) {
        self.consecutive_failures = 0;
    }
    pub fn record_execution_failure(&mut self, now: DateTime<Utc>) -> bool {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.consecutive_failures >= self.config.max_consecutive_failures {
            self.kill_switch.force_trip();
            true
        } else {
            false
        }
    }
    pub fn apply_loss_cooldown(&mut self, now: DateTime<Utc>) {
        self.state.cooldown_until =
            Some(now + chrono::Duration::minutes(self.config.cooldown_after_loss_minutes));
    }
    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    fn config() -> RiskConfig {
        RiskConfig {
            starting_capital_usd: dec!(100),
            max_live_capital_usd: dec!(25),
            max_concurrent_positions: 1,
            max_position_percent_of_equity: dec!(5),
            max_position_percent_of_liquidity: dec!(0.1),
            max_risk_per_trade_percent: dec!(0.5),
            max_daily_loss_percent: dec!(2),
            max_total_drawdown_before_kill_switch_pct: dec!(5),
            cooldown_after_loss_minutes: 30,
            max_slippage_bps: 100,
            min_liquidity_usd: dec!(50000),
            max_trades_per_day: 3,
            max_consecutive_failures: 3,
        }
    }
    fn now() -> DateTime<Utc> {
        Utc::now()
    }
    #[test]
    fn kill_switch_latches_when_equity_crosses_threshold_mid_position() {
        let mut k = KillSwitch::new(dec!(20), dec!(50));
        k.observe_equity(dec!(10.01));
        assert!(!k.is_tripped());
        k.observe_equity(dec!(10));
        assert!(k.is_tripped());
        k.observe_equity(dec!(19));
        assert!(k.is_tripped());
    }
    #[test]
    fn exits_are_allowed_under_latched_kill_switch() {
        let mut engine = RiskEngine::new(config(), dec!(100));
        engine.kill_switch.force_trip();
        assert!(engine.authorize_exit(1000).is_ok());
        assert!(engine
            .authorize(dec!(5), dec!(100000), 10, 10, now())
            .is_err());
    }
    #[test]
    fn consecutive_failures_trip_the_kill_switch() {
        let mut engine = RiskEngine::new(config(), dec!(100));
        assert!(!engine.record_execution_failure(now()));
        assert!(!engine.record_execution_failure(now()));
        assert!(engine.record_execution_failure(now()));
        assert!(engine.kill_switch.is_tripped());
        engine.record_execution_success();
        assert!(engine.kill_switch.is_tripped(), "success must not unlatch");
    }
    #[test]
    fn unknown_liquidity_or_slippage_fails_closed() {
        let engine = RiskEngine::new(config(), dec!(100));
        assert!(engine
            .authorize(dec!(5), Decimal::ZERO, 10, 10, now())
            .is_err());
        assert!(engine
            .authorize(dec!(5), dec!(100000), 200, 10, now())
            .is_err());
        assert!(engine
            .authorize(dec!(5), dec!(100000), 10, 200, now())
            .is_err());
        assert!(engine
            .authorize(dec!(5), dec!(100000), 10, 10, now())
            .is_ok());
    }
    #[test]
    fn daily_loss_and_trade_count_limits_bind() {
        let mut engine = RiskEngine::new(config(), dec!(100));
        engine.state.day_start_equity_usd = dec!(100);
        engine.state.equity_usd = dec!(97);
        assert!(
            engine
                .authorize(dec!(5), dec!(100000), 10, 10, now())
                .is_err(),
            "3% daily loss must exceed the 2% limit"
        );
        let mut engine = RiskEngine::new(config(), dec!(100));
        engine.state.trades_today = 3;
        assert!(engine
            .authorize(dec!(5), dec!(100000), 10, 10, now())
            .is_err());
    }
    #[test]
    fn cooldown_blocks_entries_only() {
        let mut engine = RiskEngine::new(config(), dec!(100));
        engine.apply_loss_cooldown(now());
        assert!(engine
            .authorize(dec!(5), dec!(100000), 10, 10, now())
            .is_err());
        assert!(engine.authorize_exit(1).is_ok());
    }
}
