use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketSnapshot {
    pub mint: String,
    pub price_usd: Decimal,
    pub liquidity_usd: Decimal,
    pub volume_24h_usd: Decimal,
    pub volatility_pct: Decimal,
    pub buy_sell_imbalance: Decimal,
    pub observed_at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
    pub slot: Option<u64>,
    #[serde(default)]
    pub price_impact_bps: Option<u32>,
}
impl MarketSnapshot {
    pub fn age_seconds(&self, now: DateTime<Utc>) -> i64 {
        (now - self.observed_at).num_seconds()
    }
}
