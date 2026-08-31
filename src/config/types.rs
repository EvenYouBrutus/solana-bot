use rust_decimal::Decimal;
use serde::Deserialize;

use super::ConfigError;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub mode: Mode,
    pub rpc: RpcConfig,
    pub strategy: StrategyConfig,
    pub economics: EconomicsConfig,
    pub risk: RiskConfig,
    pub execution: ExecutionConfig,
    pub storage: StorageConfig,
    #[serde(default)]
    pub runtime: RuntimeConfig,
    #[serde(default)]
    pub observability: ObservabilityConfig,
}
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Paper,
    Live,
    Replay,
}
#[derive(Debug, Clone, Deserialize)]
pub struct RpcConfig {
    pub http_endpoints: Vec<String>,
    #[serde(default)]
    pub websocket_endpoints: Vec<String>,
    #[serde(default = "default_stale")]
    pub max_data_age_secs: i64,
    #[serde(default = "default_timeout")]
    pub request_timeout_secs: u64,
    /// Bounded retry passes over every endpoint before giving up. Never infinite.
    #[serde(default = "default_rpc_attempts")]
    pub max_attempts: u32,
    /// An order whose signature has no on-chain status after this many seconds
    /// (blockhash expiry window) may be marked Expired instead of Unknown.
    #[serde(default = "default_unknown_after")]
    pub unknown_after_secs: i64,
}
#[derive(Debug, Clone, Deserialize)]
pub struct StrategyConfig {
    pub base_mint: String,
    pub min_wallet_score: Decimal,
    pub min_wallet_samples: u32,
    pub min_consensus_wallets: usize,
    pub min_signal_score: Decimal,
    pub min_token_age_secs: i64,
    pub stop_loss_pct: Decimal,
    pub take_profit_pct: Decimal,
    pub trailing_stop_pct: Decimal,
    pub max_holding_minutes: i64,
}
#[derive(Debug, Clone, Deserialize)]
pub struct EconomicsConfig {
    pub round_trip_cost_threshold_pct: Decimal,
    pub min_expected_net_return_pct: Decimal,
    pub max_quote_age_secs: i64,
    pub uncertainty_haircut_pct: Decimal,
    /// Price of 1 SOL in USD used to convert on-chain fee lamports into USD
    /// fee accounting. When absent, fees are still recorded in lamports but
    /// USD fee figures are zero and PnL is explicitly fee-free (logged).
    #[serde(default)]
    pub sol_price_usd: Option<Decimal>,
}
#[derive(Debug, Clone, Deserialize)]
pub struct RiskConfig {
    pub starting_capital_usd: Decimal,
    pub max_live_capital_usd: Decimal,
    pub max_concurrent_positions: usize,
    pub max_position_percent_of_equity: Decimal,
    pub max_position_percent_of_liquidity: Decimal,
    pub max_risk_per_trade_percent: Decimal,
    pub max_daily_loss_percent: Decimal,
    pub max_total_drawdown_before_kill_switch_pct: Decimal,
    pub cooldown_after_loss_minutes: i64,
    pub max_slippage_bps: u32,
    pub min_liquidity_usd: Decimal,
    pub max_trades_per_day: u32,
    /// Latch the kill switch after this many consecutive non-confirmed
    /// executions (submission errors, shortfalls, unresolvable unknowns).
    #[serde(default = "default_max_failures")]
    pub max_consecutive_failures: u32,
}
#[derive(Debug, Clone, Deserialize)]
pub struct ExecutionConfig {
    pub provider: ExecutionProvider,
    pub jupiter_api_url: String,
    pub slippage_bps: u16,
    pub priority_fee_lamports: u64,
    /// Hard cap on the total network fee (base + priority) charged by a
    /// confirmed swap. Breaches are recorded and counted as failures.
    #[serde(default = "default_max_fee")]
    pub max_fee_lamports: u64,
    /// Reject quotes whose price impact exceeds this ceiling, independently
    /// of the slippage tolerance.
    #[serde(default = "default_price_impact")]
    pub max_price_impact_bps: u32,
    /// Maximum wall-clock time to wait for a submitted transaction to reach a
    /// confirmed/finalized status before declaring the outcome Unknown.
    #[serde(default = "default_confirm_timeout")]
    pub confirm_timeout_secs: u64,
    #[serde(default = "default_confirm_poll_ms")]
    pub confirm_poll_ms: u64,
    #[serde(default)]
    pub live_signer_env: Option<String>,
    #[serde(default)]
    pub allowed_program_ids: Vec<String>,
}
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionProvider {
    Jupiter,
}
#[derive(Debug, Clone, Deserialize)]
pub struct StorageConfig {
    pub sqlite_path: String,
}
#[derive(Debug, Clone, Deserialize)]
pub struct RuntimeConfig {
    #[serde(default)]
    pub signal_feed_path: Option<String>,
    #[serde(default = "default_poll")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_paper_haircut")]
    pub paper_fill_haircut_bps: u32,
    /// How often (seconds) to re-reconcile open positions against on-chain
    /// token balances while running. Zero disables periodic re-reconciliation
    /// (startup reconciliation still runs).
    #[serde(default = "default_reconcile_interval")]
    pub reconcile_interval_secs: u64,
    /// Collector configuration for Phase 1
    #[serde(default = "default_collector")]
    pub collector: CollectorConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CollectorConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_websocket")]
    pub websocket_endpoints: Vec<String>,
    #[serde(default = "default_replay")]
    pub replay_mode: bool,
}

fn default_collector() -> CollectorConfig {
    CollectorConfig {
        enabled: true,
        websocket_endpoints: vec![],
        replay_mode: false,
    }
}
fn default_websocket() -> Vec<String> {
    vec!["wss://api.mainnet-beta.solana.com".to_string()]
}
fn default_replay() -> bool {
    true
}
#[derive(Debug, Clone, Deserialize)]
pub struct ObservabilityConfig {
    #[serde(default = "default_log")]
    pub log_format: String,
}
fn default_stale() -> i64 {
    15
}
fn default_timeout() -> u64 {
    8
}
fn default_log() -> String {
    "json".into()
}
fn default_poll() -> u64 {
    2
}
fn default_paper_haircut() -> u32 {
    25
}
fn default_rpc_attempts() -> u32 {
    2
}
fn default_unknown_after() -> i64 {
    180
}
fn default_max_failures() -> u32 {
    3
}
fn default_max_fee() -> u64 {
    500_000
}
fn default_price_impact() -> u32 {
    300
}
fn default_confirm_timeout() -> u64 {
    90
}
fn default_confirm_poll_ms() -> u64 {
    500
}
fn default_reconcile_interval() -> u64 {
    60
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            log_format: default_log(),
        }
    }
}
impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            signal_feed_path: None,
            poll_interval_secs: default_poll(),
            paper_fill_haircut_bps: default_paper_haircut(),
            reconcile_interval_secs: default_reconcile_interval(),
            collector: default_collector(),
        }
    }
}
impl Config {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.rpc.http_endpoints.is_empty() {
            return Err(ConfigError::Invalid(
                "at least one HTTP RPC endpoint is required".into(),
            ));
        }
        if self.rpc.max_attempts == 0 {
            return Err(ConfigError::Invalid(
                "rpc.max_attempts must be at least 1".into(),
            ));
        }
        if self.strategy.min_consensus_wallets < 2 {
            return Err(ConfigError::Invalid(
                "min_consensus_wallets must be at least 2".into(),
            ));
        }
        if self.risk.max_risk_per_trade_percent <= Decimal::ZERO
            || self.risk.max_risk_per_trade_percent > Decimal::new(100, 0)
        {
            return Err(ConfigError::Invalid(
                "max_risk_per_trade_percent must be in (0,100]".into(),
            ));
        }
        if self.risk.max_slippage_bps == 0
            || self.execution.slippage_bps as u32 > self.risk.max_slippage_bps
        {
            return Err(ConfigError::Invalid(
                "execution slippage must be positive and no greater than risk maximum".into(),
            ));
        }
        if self.risk.max_consecutive_failures == 0 {
            return Err(ConfigError::Invalid(
                "max_consecutive_failures must be positive".into(),
            ));
        }
        if self.economics.min_expected_net_return_pct <= Decimal::ZERO
            || self.risk.min_liquidity_usd <= Decimal::ZERO
        {
            return Err(ConfigError::Invalid(
                "minimum edge and liquidity must be positive".into(),
            ));
        }
        if let Some(price) = self.economics.sol_price_usd {
            if price <= Decimal::ZERO {
                return Err(ConfigError::Invalid(
                    "sol_price_usd must be positive when provided".into(),
                ));
            }
        }
        if self.execution.max_fee_lamports
            < self.execution.priority_fee_lamports.saturating_add(5_000)
        {
            return Err(ConfigError::Invalid(
                "max_fee_lamports must cover base fee plus configured priority fee".into(),
            ));
        }
        if self.execution.max_price_impact_bps == 0 || self.execution.max_price_impact_bps > 10_000
        {
            return Err(ConfigError::Invalid(
                "max_price_impact_bps must be in (0,10000]".into(),
            ));
        }
        if self.execution.confirm_timeout_secs < 5 {
            return Err(ConfigError::Invalid(
                "confirm_timeout_secs must be at least 5".into(),
            ));
        }
        if self.execution.confirm_poll_ms == 0 {
            return Err(ConfigError::Invalid(
                "confirm_poll_ms must be positive".into(),
            ));
        }
        if self.mode == Mode::Live
            && (self
                .execution
                .live_signer_env
                .as_deref()
                .unwrap_or_default()
                .is_empty()
                || self.risk.max_live_capital_usd <= Decimal::ZERO
                || self.risk.max_live_capital_usd > self.risk.starting_capital_usd)
        {
            return Err(ConfigError::Invalid("live mode requires signer and a positive capital cap no greater than starting capital".into()));
        }
        if self.mode == Mode::Live && self.execution.jupiter_api_url.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "live mode requires a non-empty jupiter_api_url".into(),
            ));
        }
        if self.mode == Mode::Live && self.execution.allowed_program_ids.is_empty() {
            return Err(ConfigError::Invalid(
                "live mode requires a non-empty execution program allowlist".into(),
            ));
        }
        if self.mode == Mode::Live && self.runtime.reconcile_interval_secs == 0 {
            return Err(ConfigError::Invalid(
                "live mode requires periodic on-chain position reconciliation".into(),
            ));
        }
        if self.runtime.poll_interval_secs == 0
            || self.economics.max_quote_age_secs <= 0
            || self.rpc.max_data_age_secs <= 0
            || self.execution.slippage_bps > 10_000
        {
            return Err(ConfigError::Invalid(
                "poll, freshness intervals, and slippage must be valid".into(),
            ));
        }
        if self.strategy.stop_loss_pct <= Decimal::ZERO
            || self.strategy.take_profit_pct <= Decimal::ZERO
            || self.strategy.trailing_stop_pct < Decimal::ZERO
        {
            return Err(ConfigError::Invalid(
                "stop-loss and take-profit must be positive; trailing stop non-negative".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> Config {
        let text = r#"
mode = "paper"
[rpc]
http_endpoints = ["https://api.test"]
[strategy]
base_mint = "So11111111111111111111111111111111111111112"
min_wallet_score = 60.0
min_wallet_samples = 25
min_consensus_wallets = 2
min_signal_score = 65.0
min_token_age_secs = 86400
stop_loss_pct = 5.0
take_profit_pct = 12.0
trailing_stop_pct = 4.0
max_holding_minutes = 240
[economics]
round_trip_cost_threshold_pct = 3.0
min_expected_net_return_pct = 2.0
max_quote_age_secs = 3
uncertainty_haircut_pct = 1.0
[risk]
starting_capital_usd = 100.0
max_live_capital_usd = 25.0
max_concurrent_positions = 1
max_position_percent_of_equity = 5.0
max_position_percent_of_liquidity = 0.10
max_risk_per_trade_percent = 0.5
max_daily_loss_percent = 2.0
max_total_drawdown_before_kill_switch_pct = 5.0
cooldown_after_loss_minutes = 30
max_slippage_bps = 100
min_liquidity_usd = 50000.0
max_trades_per_day = 3
[execution]
provider = "jupiter"
jupiter_api_url = "https://api.jup.ag"
slippage_bps = 75
priority_fee_lamports = 10000
allowed_program_ids = []
[storage]
sqlite_path = ":memory:"
"#;
        toml::from_str(text).expect("valid test config")
    }
    #[test]
    fn defaults_are_conservative_and_valid() {
        assert!(config().validate().is_ok());
    }
    #[test]
    fn fee_cap_must_cover_base_and_priority() {
        let mut c = config();
        c.execution.max_fee_lamports = 10_000;
        assert!(c.validate().is_err());
    }
    #[test]
    fn live_mode_still_requires_signer_and_allowlist() {
        let mut c = config();
        c.mode = Mode::Live;
        assert!(c.validate().is_err());
        c.execution.live_signer_env = Some("KEY".into());
        assert!(c.validate().is_err());
        c.execution.allowed_program_ids =
            vec!["JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4".into()];
        c.runtime.reconcile_interval_secs = 0;
        assert!(c.validate().is_err());
        c.runtime.reconcile_interval_secs = 60;
        assert!(c.validate().is_ok());
    }
    #[test]
    fn live_mode_requires_jupiter_api_url() {
        let mut c = config();
        c.mode = Mode::Live;
        c.execution.live_signer_env = Some("KEY".into());
        c.execution.allowed_program_ids =
            vec!["JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4".into()];
        c.runtime.reconcile_interval_secs = 60;
        assert!(c.validate().is_ok());
        c.execution.jupiter_api_url = "".into();
        assert!(
            c.validate().is_err(),
            "live mode must reject empty jupiter_api_url"
        );
    }
    #[test]
    fn negative_sol_price_is_rejected() {
        let mut c = config();
        c.economics.sol_price_usd = Some(rust_decimal_macros::dec!(-1));
        assert!(c.validate().is_err());
    }
}
