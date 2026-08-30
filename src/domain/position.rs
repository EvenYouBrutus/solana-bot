use chrono::{DateTime, Utc}; use rust_decimal::Decimal; use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize)] pub struct Position { pub mint: String, pub quantity: Decimal, pub entry_price_usd: Decimal, pub entry_time: DateTime<Utc>, pub high_water_price_usd: Decimal, pub realized_pnl_usd: Decimal, pub fees_usd: Decimal, pub signal_id: String }
