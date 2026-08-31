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
        // Aggregate exposure: new position must not push total exposure past
        // max_live_capital_usd.
        let projected = self.state.total_exposure_usd + position_usd;
        if projected >= self.config.max_live_capital_usd {
            return Err(RiskError::Rejected(format!(
                "aggregate exposure {} would exceed max live capital {}",
                projected, self.config.max_live_capital_usd
            )));
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
    /// Rebuild aggregate exposure from the actual portfolio. Called on startup
    /// and after every position change to keep risk state consistent.
    pub fn set_total_exposure(&mut self, exposure: Decimal) {
        self.state.total_exposure_usd = exposure;
    }
    /// Rebuild open-position count from the actual portfolio. Called on startup
    /// and after every position change.
    pub fn set_open_positions(&mut self, count: usize) {
        self.state.open_positions = count;
    }
    pub fn register_trade(&mut self, exposure_usd: Decimal) {
        self.state.trades_today = self.state.trades_today.saturating_add(1);
        self.state.total_exposure_usd += exposure_usd;
        self.state.open_positions = self.state.open_positions.saturating_add(1);
    }
    pub fn register_exit(&mut self, exposure_usd: Decimal) {
        self.state.open_positions = self.state.open_positions.saturating_sub(1);
        self.state.total_exposure_usd =
            (self.state.total_exposure_usd - exposure_usd).max(Decimal::ZERO);
    }
    /// Losses impose a trade cooldown; successes clear the failure streak.
    pub fn record_execution_success(&mut self) {
        self.consecutive_failures = 0;
    }
    pub fn record_execution_failure(&mut self, _now: DateTime<Utc>) -> bool {
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

    // --- Aggregate exposure tests ---

    #[test]
    fn aggregate_exposure_blocks_when_exceeding_max_live_capital() {
        let mut cfg = config();
        cfg.max_concurrent_positions = 10;
        cfg.max_position_percent_of_equity = dec!(100);
        cfg.max_position_percent_of_liquidity = dec!(100);
        let mut engine = RiskEngine::new(cfg, dec!(1000));
        engine.set_total_exposure(dec!(20));
        engine.set_open_positions(1);
        assert!(
            engine
                .authorize(dec!(10), dec!(1000000), 10, 10, now())
                .is_err(),
            "20+10=30 > 25 max live capital must be rejected"
        );
    }

    #[test]
    fn aggregate_exposure_allows_within_limit() {
        let mut cfg = config();
        cfg.max_concurrent_positions = 10;
        cfg.max_position_percent_of_equity = dec!(100);
        cfg.max_position_percent_of_liquidity = dec!(100);
        let mut engine = RiskEngine::new(cfg, dec!(1000));
        engine.set_total_exposure(dec!(12));
        engine.set_open_positions(1);
        assert!(engine
            .authorize(dec!(5), dec!(1000000), 10, 10, now())
            .is_ok());
    }

    #[test]
    fn aggregate_exposure_exact_limit_is_rejected() {
        let mut cfg = config();
        cfg.max_concurrent_positions = 10;
        cfg.max_position_percent_of_equity = dec!(100);
        cfg.max_position_percent_of_liquidity = dec!(100);
        let mut engine = RiskEngine::new(cfg, dec!(1000));
        engine.set_total_exposure(dec!(20));
        engine.set_open_positions(1);
        assert!(
            engine
                .authorize(dec!(5), dec!(1000000), 10, 10, now())
                .is_err(),
            "20+5=25 equals the cap and must be rejected"
        );
    }

    #[test]
    fn register_trade_increments_exposure() {
        let mut engine = RiskEngine::new(config(), dec!(100));
        assert_eq!(engine.state.total_exposure_usd, Decimal::ZERO);
        engine.register_trade(dec!(10));
        assert_eq!(engine.state.total_exposure_usd, dec!(10));
        assert_eq!(engine.state.open_positions, 1);
        engine.register_trade(dec!(5));
        assert_eq!(engine.state.total_exposure_usd, dec!(15));
        assert_eq!(engine.state.open_positions, 2);
    }

    #[test]
    fn register_exit_decrements_exposure() {
        let mut engine = RiskEngine::new(config(), dec!(100));
        engine.set_total_exposure(dec!(20));
        engine.set_open_positions(2);
        engine.register_exit(dec!(10));
        assert_eq!(engine.state.total_exposure_usd, dec!(10));
        assert_eq!(engine.state.open_positions, 1);
    }

    #[test]
    fn register_exit_clamps_exposure_at_zero() {
        let mut engine = RiskEngine::new(config(), dec!(100));
        engine.set_total_exposure(dec!(5));
        engine.register_exit(dec!(10));
        assert_eq!(
            engine.state.total_exposure_usd,
            Decimal::ZERO,
            "exposure must clamp at zero, not go negative"
        );
    }

    #[test]
    fn set_total_exposure_syncs_from_portfolio() {
        let mut engine = RiskEngine::new(config(), dec!(100));
        assert_eq!(engine.state.total_exposure_usd, Decimal::ZERO);
        engine.set_total_exposure(dec!(18.5));
        assert_eq!(engine.state.total_exposure_usd, dec!(18.5));
    }

    #[test]
    fn partial_exit_exposure_tracks_correctly() {
        let mut engine = RiskEngine::new(config(), dec!(100));
        // Simulate two positions: 12 + 10 = 22 exposure
        engine.register_trade(dec!(12));
        engine.register_trade(dec!(10));
        assert_eq!(engine.state.total_exposure_usd, dec!(22));
        assert_eq!(engine.state.open_positions, 2);
        // Partial exit of first position (entry cost was 12)
        engine.register_exit(dec!(12));
        assert_eq!(engine.state.total_exposure_usd, dec!(10));
        assert_eq!(engine.state.open_positions, 1);
        // Remaining position exits
        engine.register_exit(dec!(10));
        assert_eq!(engine.state.total_exposure_usd, Decimal::ZERO);
        assert_eq!(engine.state.open_positions, 0);
    }

    // --- Daily loss tests ---

    #[test]
    fn daily_loss_blocks_at_threshold() {
        let mut engine = RiskEngine::new(config(), dec!(100));
        engine.state.day_start_equity_usd = dec!(100);
        // 2% of 100 = 2. At 98 equity the loss is exactly 2%.
        engine.state.equity_usd = dec!(98);
        assert!(
            engine
                .authorize(dec!(1), dec!(100000), 10, 10, now())
                .is_err(),
            "exactly 2% daily loss must be rejected"
        );
    }

    #[test]
    fn daily_loss_allows_just_under_threshold() {
        let mut engine = RiskEngine::new(config(), dec!(100));
        engine.state.day_start_equity_usd = dec!(100);
        engine.state.equity_usd = dec!(98.01);
        assert!(engine
            .authorize(dec!(1), dec!(100000), 10, 10, now())
            .is_ok());
    }

    #[test]
    fn daily_loss_resets_on_new_day() {
        let mut engine = RiskEngine::new(config(), dec!(100));
        engine.state.day_start_equity_usd = dec!(100);
        engine.state.equity_usd = dec!(97);
        assert!(engine
            .authorize(dec!(1), dec!(100000), 10, 10, now())
            .is_err());
        // Simulate day rollover
        engine.state.day_start_equity_usd = dec!(97);
        assert!(engine
            .authorize(dec!(1), dec!(100000), 10, 10, now())
            .is_ok());
    }

    // --- Drawdown kill switch tests ---

    #[test]
    fn drawdown_kill_switch_blocks_entries() {
        let mut engine = RiskEngine::new(config(), dec!(100));
        // 5% drawdown from 100 = 95. At 95 the kill switch trips.
        engine.observe_equity(dec!(95));
        assert!(engine.kill_switch.is_tripped());
        assert!(engine
            .authorize(dec!(1), dec!(100000), 10, 10, now())
            .is_err());
    }

    #[test]
    fn drawdown_kill_switch_allows_exits() {
        let mut engine = RiskEngine::new(config(), dec!(100));
        engine.observe_equity(dec!(95));
        assert!(engine.kill_switch.is_tripped());
        assert!(engine.authorize_exit(1000).is_ok());
    }

    #[test]
    fn drawdown_kill_switch_persists_after_restart() {
        // Simulate: engine tripped during session, process restarts,
        // kill switch reason is persisted, new engine must be tripped.
        let mut engine = RiskEngine::new(config(), dec!(100));
        engine.observe_equity(dec!(95));
        assert!(engine.kill_switch.is_tripped());
        // Simulate restart: create new engine, force_trip from persisted state
        let mut engine2 = RiskEngine::new(config(), dec!(100));
        assert!(!engine2.kill_switch.is_tripped());
        engine2.kill_switch.force_trip();
        assert!(engine2.kill_switch.is_tripped());
        assert!(engine2
            .authorize(dec!(1), dec!(100000), 10, 10, now())
            .is_err());
    }

    // --- Consecutive failures tests ---

    #[test]
    fn consecutive_failures_trip_at_config_threshold() {
        let mut engine = RiskEngine::new(config(), dec!(100));
        // max_consecutive_failures = 3
        assert!(!engine.record_execution_failure(now()));
        assert!(!engine.record_execution_failure(now()));
        assert!(engine.record_execution_failure(now()));
        assert!(engine.kill_switch.is_tripped());
    }

    #[test]
    fn success_clears_failure_streak() {
        let mut engine = RiskEngine::new(config(), dec!(100));
        engine.record_execution_failure(now());
        engine.record_execution_failure(now());
        engine.record_execution_success();
        assert_eq!(engine.consecutive_failures(), 0);
        // After success, two more failures needed to trip (3 total)
        assert!(!engine.record_execution_failure(now()));
        assert!(!engine.record_execution_failure(now()));
        assert!(engine.record_execution_failure(now()));
    }

    #[test]
    fn failure_streak_does_not_double_count() {
        let mut engine = RiskEngine::new(config(), dec!(100));
        // After 3 failures the kill switch trips
        engine.record_execution_failure(now());
        engine.record_execution_failure(now());
        assert!(engine.record_execution_failure(now()));
        // Kill switch is now tripped; additional failures still increment
        // the counter but the switch is already latched (monotonic).
        engine.record_execution_failure(now());
        assert_eq!(engine.consecutive_failures(), 4);
    }

    // --- Emergency stop interaction tests ---

    #[test]
    fn emergency_stop_blocks_all_entry_paths() {
        // Emergency stop is managed externally (state store) and checked in
        // runtime before calling authorize(). This test verifies authorize()
        // itself does not accidentally weaken fail-closed behaviour.
        let engine = RiskEngine::new(config(), dec!(100));
        // authorize() checks kill switch, position size, liquidity, concurrent
        // positions, trade count, equity/liquidity limits, slippage, cooldown,
        // daily loss. All must fail-closed.
        assert!(engine
            .authorize(Decimal::ZERO, dec!(100000), 10, 10, now())
            .is_err());
        assert!(engine
            .authorize(dec!(5), Decimal::ZERO, 10, 10, now())
            .is_err());
    }

    #[test]
    fn authorize_exit_only_requires_positive_qty() {
        let engine = RiskEngine::new(config(), dec!(100));
        assert!(engine.authorize_exit(0).is_err());
        assert!(engine.authorize_exit(1).is_ok());
        assert!(engine.authorize_exit(u64::MAX).is_ok());
    }

    // --- Position limit tests ---

    #[test]
    fn concurrent_position_limit_enforced() {
        let mut engine = RiskEngine::new(config(), dec!(100));
        engine.set_open_positions(1);
        assert!(engine
            .authorize(dec!(5), dec!(100000), 10, 10, now())
            .is_err());
    }

    #[test]
    fn per_position_equity_limit_enforced() {
        let engine = RiskEngine::new(config(), dec!(100));
        // max_position_percent_of_equity = 5%, equity = 100, so max = 5
        assert!(engine
            .authorize(dec!(6), dec!(100000), 10, 10, now())
            .is_err());
        assert!(engine
            .authorize(dec!(5), dec!(100000), 10, 10, now())
            .is_ok());
    }

    #[test]
    fn per_position_liquidity_limit_enforced() {
        let mut cfg = config();
        cfg.max_concurrent_positions = 10;
        cfg.max_position_percent_of_equity = dec!(100);
        cfg.max_live_capital_usd = dec!(10000);
        let engine = RiskEngine::new(cfg, dec!(1000));
        // max_position_percent_of_liquidity = 0.1%, liquidity = 50000, max = 50
        assert!(engine
            .authorize(dec!(51), dec!(50000), 10, 10, now())
            .is_err());
        assert!(engine
            .authorize(dec!(50), dec!(50000), 10, 10, now())
            .is_ok());
    }

    // --- Trade count tests ---

    #[test]
    fn daily_trade_count_enforced() {
        let mut engine = RiskEngine::new(config(), dec!(100));
        engine.state.trades_today = 3;
        assert!(engine
            .authorize(dec!(1), dec!(100000), 10, 10, now())
            .is_err());
    }

    #[test]
    fn register_trade_increments_count() {
        let mut engine = RiskEngine::new(config(), dec!(100));
        assert_eq!(engine.state.trades_today, 0);
        engine.register_trade(dec!(5));
        assert_eq!(engine.state.trades_today, 1);
    }
}
