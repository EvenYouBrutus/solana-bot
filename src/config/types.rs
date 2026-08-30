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
    #[serde(default)] pub runtime: RuntimeConfig,
    #[serde(default)] pub observability: ObservabilityConfig,
}
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode { Paper, Live, Replay }
#[derive(Debug, Clone, Deserialize)]
pub struct RpcConfig { pub http_endpoints: Vec<String>, #[serde(default)] pub websocket_endpoints: Vec<String>, #[serde(default = "default_stale")] pub max_data_age_secs: i64, #[serde(default = "default_timeout")] pub request_timeout_secs: u64 }
#[derive(Debug, Clone, Deserialize)]
pub struct StrategyConfig { pub base_mint: String, pub min_wallet_score: Decimal, pub min_wallet_samples: u32, pub min_consensus_wallets: usize, pub min_signal_score: Decimal, pub min_token_age_secs: i64, pub stop_loss_pct: Decimal, pub take_profit_pct: Decimal, pub trailing_stop_pct: Decimal, pub max_holding_minutes: i64 }
#[derive(Debug, Clone, Deserialize)]
pub struct EconomicsConfig { pub round_trip_cost_threshold_pct: Decimal, pub min_expected_net_return_pct: Decimal, pub max_quote_age_secs: i64, pub uncertainty_haircut_pct: Decimal }
#[derive(Debug, Clone, Deserialize)]
pub struct RiskConfig { pub starting_capital_usd: Decimal, pub max_live_capital_usd: Decimal, pub max_concurrent_positions: usize, pub max_position_percent_of_equity: Decimal, pub max_position_percent_of_liquidity: Decimal, pub max_risk_per_trade_percent: Decimal, pub max_daily_loss_percent: Decimal, pub max_total_drawdown_before_kill_switch_pct: Decimal, pub cooldown_after_loss_minutes: i64, pub max_slippage_bps: u32, pub min_liquidity_usd: Decimal, pub max_trades_per_day: u32 }
#[derive(Debug, Clone, Deserialize)]
pub struct ExecutionConfig { pub provider: ExecutionProvider, pub jupiter_api_url: String, pub slippage_bps: u16, pub priority_fee_lamports: u64, #[serde(default)] pub live_signer_env: Option<String>, #[serde(default)] pub allowed_program_ids: Vec<String> }
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionProvider { Jupiter }
#[derive(Debug, Clone, Deserialize)]
pub struct StorageConfig { pub sqlite_path: String }
#[derive(Debug, Clone, Deserialize)]
pub struct RuntimeConfig { #[serde(default)] pub signal_feed_path: Option<String>, #[serde(default = "default_poll")] pub poll_interval_secs: u64, #[serde(default = "default_paper_haircut")] pub paper_fill_haircut_bps: u32 }
#[derive(Debug, Clone, Deserialize)]
pub struct ObservabilityConfig { #[serde(default = "default_log")] pub log_format: String }
fn default_stale() -> i64 { 15 } fn default_timeout() -> u64 { 8 } fn default_log() -> String { "json".into() }
fn default_poll() -> u64 { 2 } fn default_paper_haircut() -> u32 { 25 }

impl Default for ObservabilityConfig { fn default() -> Self { Self { log_format: default_log() } } }
impl Default for RuntimeConfig { fn default() -> Self { Self { signal_feed_path: None, poll_interval_secs: default_poll(), paper_fill_haircut_bps: default_paper_haircut() } } }
impl Config {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.rpc.http_endpoints.is_empty() { return Err(ConfigError::Invalid("at least one HTTP RPC endpoint is required".into())); }
        if self.strategy.min_consensus_wallets < 2 { return Err(ConfigError::Invalid("min_consensus_wallets must be at least 2".into())); }
        if self.risk.max_risk_per_trade_percent <= Decimal::ZERO || self.risk.max_risk_per_trade_percent > Decimal::new(100, 0) { return Err(ConfigError::Invalid("max_risk_per_trade_percent must be in (0,100]".into())); }
        if self.risk.max_slippage_bps == 0 || self.execution.slippage_bps as u32 > self.risk.max_slippage_bps { return Err(ConfigError::Invalid("execution slippage must be positive and no greater than risk maximum".into())); }
        if self.economics.min_expected_net_return_pct <= Decimal::ZERO || self.risk.min_liquidity_usd <= Decimal::ZERO { return Err(ConfigError::Invalid("minimum edge and liquidity must be positive".into())); }
        if self.mode == Mode::Live && (self.execution.live_signer_env.as_deref().unwrap_or_default().is_empty() || self.risk.max_live_capital_usd <= Decimal::ZERO || self.risk.max_live_capital_usd > self.risk.starting_capital_usd) { return Err(ConfigError::Invalid("live mode requires signer and a positive capital cap no greater than starting capital".into())); }
        if self.mode == Mode::Live && self.execution.allowed_program_ids.is_empty() { return Err(ConfigError::Invalid("live mode requires a non-empty execution program allowlist".into())); }
        if self.runtime.poll_interval_secs == 0 || self.economics.max_quote_age_secs <= 0 || self.rpc.max_data_age_secs <= 0 || self.execution.slippage_bps > 10_000 { return Err(ConfigError::Invalid("poll, freshness intervals, and slippage must be valid".into())); }
        Ok(())
    }
}
