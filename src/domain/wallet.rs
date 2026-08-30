use chrono::{DateTime, Utc}; use rust_decimal::Decimal; use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)] pub enum WalletTier { Candidate, Observed, Qualified, HighConfidence }
#[derive(Debug, Clone, Serialize, Deserialize)] pub struct WalletStats { pub wallet: String, pub realized_pnl_usd: Decimal, pub win_rate: Decimal, pub avg_return_pct: Decimal, pub median_return_pct: Decimal, pub max_drawdown_pct: Decimal, pub trades: u32, pub recent_return_pct: Decimal, pub concentration_pct: Decimal, pub scam_exposure_pct: Decimal, pub score: Decimal, pub tier: WalletTier, pub updated_at: DateTime<Utc> }
#[derive(Debug, Clone, Serialize, Deserialize)] pub struct WalletTradeObservation { pub wallet: String, pub mint: String, pub side: Side, pub notional_usd: Decimal, pub observed_at: DateTime<Utc>, pub signature: String }
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)] pub enum Side { Buy, Sell }
