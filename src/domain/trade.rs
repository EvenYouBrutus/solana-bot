use chrono::{DateTime, Utc}; use rust_decimal::Decimal; use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize)] pub struct Fill { pub order_id: String, pub signature: String, pub input_amount: u64, pub output_amount: u64, pub price_usd: Decimal, pub fees_usd: Decimal, pub slippage_bps: u32, pub confirmed_at: DateTime<Utc>, pub latency_ms: u64 }
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)] pub enum OrderState { Pending, Submitted, Confirmed, Failed, Dropped, Unknown, Expired }
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)] pub enum OrderKind { Entry, Exit, LegacyUnknown }
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)] pub enum OrderSide { Buy, Sell, Unknown }
fn legacy_order_kind() -> OrderKind { OrderKind::LegacyUnknown }
fn legacy_order_side() -> OrderSide { OrderSide::Unknown }
#[derive(Debug, Clone, Serialize, Deserialize)] pub struct OrderRecord { pub id: String, pub signal_id: String, pub mint: String, #[serde(default = "legacy_order_kind")] pub kind: OrderKind, #[serde(default)] pub position_id: Option<String>, #[serde(default = "legacy_order_side")] pub side: OrderSide, #[serde(default)] pub input_mint: Option<String>, #[serde(default)] pub output_mint: Option<String>, #[serde(default)] pub input_amount_atomic: Option<u64>, pub state: OrderState, pub idempotency_key: String, pub created_at: DateTime<Utc>, pub signature: Option<String>, pub error: Option<String> }
