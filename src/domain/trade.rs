use chrono::{DateTime, Utc}; use rust_decimal::Decimal; use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize)] pub struct Fill { pub order_id: String, pub signature: String, pub input_amount: u64, pub output_amount: u64, pub price_usd: Decimal, pub fees_usd: Decimal, pub slippage_bps: u32, pub confirmed_at: DateTime<Utc>, pub latency_ms: u64 }
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)] pub enum OrderState { Pending, Submitted, Confirmed, Failed, Expired }
#[derive(Debug, Clone, Serialize, Deserialize)] pub struct OrderRecord { pub id: String, pub signal_id: String, pub mint: String, pub state: OrderState, pub idempotency_key: String, pub created_at: DateTime<Utc>, pub error: Option<String> }
