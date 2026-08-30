use crate::domain::trade::Fill;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct Quote { pub input_mint: String, pub output_mint: String, pub input_amount: u64, pub output_amount: u64, pub price_impact_bps: u32, pub route: serde_json::Value, pub observed_at: DateTime<Utc> }

/// Value basis for a swap. Exactly one side must be provided so the fill can
/// be priced from verified on-chain amounts; refusing otherwise prevents
/// silent mis-accounting.
#[derive(Debug, Clone)]
pub enum ValueBasis {
    /// Entry: the USD value of the fixed input leg (exact-in), from the
    /// verified candidate feed.
    InputValueUsd(Decimal),
    /// Exit: the USD price of one unit of the output leg (base mint).
    OutputUnitPriceUsd(Decimal),
}
impl ValueBasis {
    /// Returns (usd_value_of_trade, usd_price_per_traded_token).
    pub fn price_fill(&self, input_amount: u64, input_decimals: u8, output_amount: u64, output_decimals: u8) -> Result<(Decimal, Decimal), ExecutionError> {
        let input_units = units(input_amount, input_decimals)?;
        let output_units = units(output_amount, output_decimals)?;
        match self {
            ValueBasis::InputValueUsd(v) => {
                if output_units <= Decimal::ZERO { return Err(ExecutionError::InvalidQuote); }
                let price = *v / output_units;
                Ok((*v, price))
            }
            ValueBasis::OutputUnitPriceUsd(p) => {
                if input_units <= Decimal::ZERO { return Err(ExecutionError::InvalidQuote); }
                let value = output_units * *p;
                let price = value / input_units;
                Ok((value, price))
            }
        }
    }
}
/// Converts an atomic amount into human units with exact Decimal math.
pub fn units(amount: u64, decimals: u8) -> Result<Decimal, ExecutionError> {
    let d = rust_decimal::Decimal::from(10u64.checked_pow(decimals as u32).ok_or_else(|| ExecutionError::Policy("decimals overflow".into()))?);
    Ok(Decimal::from(amount) / d)
}

#[derive(Debug, Clone)]
pub struct ExecutionRequest {
    pub order_id: String,
    pub quote: Quote,
    pub max_slippage_bps: u32,
    pub max_price_impact_bps: u32,
    pub min_output_amount: u64,
    pub input_decimals: u8,
    pub output_decimals: u8,
    pub value_basis: ValueBasis,
}

#[derive(Debug, Error)]
pub enum ExecutionError {
    #[error("quote unavailable: {0}")]
    Quote(String),
    #[error("quote is stale")]
    StaleQuote,
    #[error("quote violates slippage/impact/receive limit")]
    InvalidQuote,
    #[error("transaction policy rejected: {0}")]
    Policy(String),
    /// Outcome could not be determined. The signature (if any) must be
    /// reconciled on-chain before any retry is considered.
    #[error("transaction outcome is unknown; reconcile before retrying (signature={signature:?}): {detail}")]
    Unknown { signature: Option<String>, detail: String },
    #[error("transaction failed on-chain: {0}")]
    Transaction(String),
    #[error("execution backend unavailable: {0}")]
    Unavailable(String),
}

#[async_trait]
pub trait Executor: Send + Sync {
    async fn quote(&self, input_mint: &str, output_mint: &str, amount: u64, slippage_bps: u16) -> Result<Quote, ExecutionError>;
    async fn execute(&self, request: ExecutionRequest) -> Result<Fill, ExecutionError>;
    async fn health(&self) -> Result<(), ExecutionError>;
    /// Wallet public key when real signing is possible; paper mode has none.
    fn signer_pubkey(&self) -> Option<String> { None }
    fn is_live(&self) -> bool { false }
}

/// Enriches a fill with fee USD using a SOL price; lamports are always kept.
pub fn fee_usd(fee_lamports: u64, sol_price_usd: Option<Decimal>) -> Decimal {
    match sol_price_usd {
        Some(p) => Decimal::from(fee_lamports) / Decimal::from(1_000_000_000u64) * p,
        None => Decimal::ZERO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    #[test]
    fn units_convert_with_decimals() {
        assert_eq!(units(1_500_000, 6).unwrap(), dec!(1.5));
        assert_eq!(units(1, 0).unwrap(), Decimal::ONE);
        assert_eq!(units(123_456_789, 9).unwrap(), dec!(0.123456789));
        assert!(units(1, 40).is_err());
    }
    #[test]
    fn entry_pricing_uses_actual_output() {
        let (v, p) = ValueBasis::InputValueUsd(dec!(10)).price_fill(1_000_000_000, 9, 5_000_000, 6).unwrap();
        assert_eq!(v, dec!(10));
        assert_eq!(p, dec!(2));
    }
    #[test]
    fn exit_pricing_uses_actual_base_output() {
        let (v, p) = ValueBasis::OutputUnitPriceUsd(dec!(150)).price_fill(5_000_000, 6, 1_500_000_000, 9).unwrap();
        assert_eq!(v, dec!(225));
        assert_eq!(p, dec!(45));
    }
    #[test]
    fn zero_legs_are_refused() {
        assert!(ValueBasis::InputValueUsd(dec!(10)).price_fill(1, 9, 0, 6).is_err());
        assert!(ValueBasis::OutputUnitPriceUsd(dec!(1)).price_fill(0, 6, 1, 9).is_err());
    }
    #[test]
    fn fee_conversion_requires_sol_price() {
        assert_eq!(fee_usd(1_000_000, Some(dec!(150))), dec!(0.150000000));
        assert_eq!(fee_usd(1_000_000, None), Decimal::ZERO);
    }
}
