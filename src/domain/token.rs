use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct Token {
    pub mint: String,
    pub symbol: Option<String>,
    pub decimals: u8,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenSafety {
    pub mint_authority_present: bool,
    pub freeze_authority_present: bool,
    pub holder_top10_pct: Decimal,
    pub token_age_secs: i64,
    pub liquidity_locked_or_burned: Option<bool>,
    pub sellable: Option<bool>,
    pub route_available: Option<bool>,
    pub creator_suspicious: Option<bool>,
    pub abnormal_activity: Option<bool>,
    pub liquidity_change_pct: Option<Decimal>,
    pub observed_at: DateTime<Utc>,
}
