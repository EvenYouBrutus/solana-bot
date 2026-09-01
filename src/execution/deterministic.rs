use super::{ExecutionError, ExecutionRequest, Executor, Quote};
use crate::domain::trade::Fill;
use async_trait::async_trait;
use chrono::Utc;
use std::collections::HashMap;
use std::sync::Mutex;

/// A deterministic executor for replay mode. Quotes are served from a
/// pre-built map; executes always succeed with the quoted amounts (no
/// slippage, no failures). This guarantees full reproducibility.
pub struct DeterministicExecutor {
    quotes: Mutex<HashMap<(String, String, u64, u16), Quote>>,
}

impl DeterministicExecutor {
    pub fn new() -> Self {
        Self {
            quotes: Mutex::new(HashMap::new()),
        }
    }

    /// Seed the executor with a set of quotes. Overwrites any existing
    /// entries for the same (input, output, amount, slippage) tuple.
    pub fn with_quotes(self, quotes: Vec<Quote>) -> Self {
        {
            let mut map = self.quotes.lock().unwrap();
            for q in quotes {
                let key = (
                    q.input_mint.clone(),
                    q.output_mint.clone(),
                    q.input_amount,
                    0,
                );
                map.insert(key, q);
            }
        }
        self
    }

    /// Register a single quote, keyed by (input, output, amount).
    pub fn register_quote(&self, quote: Quote) {
        let key = (
            quote.input_mint.clone(),
            quote.output_mint.clone(),
            quote.input_amount,
            0,
        );
        self.quotes.lock().unwrap().insert(key, quote);
    }
}

impl Default for DeterministicExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Executor for DeterministicExecutor {
    async fn quote(
        &self,
        input_mint: &str,
        output_mint: &str,
        amount: u64,
        _slippage_bps: u16,
    ) -> Result<Quote, ExecutionError> {
        let key = (input_mint.to_string(), output_mint.to_string(), amount, 0);
        self.quotes
            .lock()
            .unwrap()
            .get(&key)
            .cloned()
            .ok_or_else(|| {
                ExecutionError::Quote(format!(
                    "no deterministic quote for {input_mint}->{output_mint} amount={amount}"
                ))
            })
    }

    async fn execute(&self, request: ExecutionRequest) -> Result<Fill, ExecutionError> {
        let price_usd = request.value_basis.price_fill(
            request.quote.input_amount,
            request.input_decimals,
            request.quote.output_amount,
            request.output_decimals,
        )?;
        Ok(Fill {
            order_id: request.order_id,
            signature: format!("replay:{}", uuid::Uuid::new_v4()),
            input_amount: request.quote.input_amount,
            output_amount: request.quote.output_amount,
            price_usd: price_usd.1,
            fees_usd: rust_decimal::Decimal::ZERO,
            slippage_bps: 0,
            confirmed_at: Utc::now(),
            latency_ms: 0,
            fee_lamports: 0,
            input_value_usd: Some(price_usd.0),
            expected_output_amount: Some(request.quote.output_amount),
        })
    }

    async fn health(&self) -> Result<(), ExecutionError> {
        Ok(())
    }

    fn register_quote(&self, quote: Quote) {
        let key = (
            quote.input_mint.clone(),
            quote.output_mint.clone(),
            quote.input_amount,
            0,
        );
        self.quotes.lock().unwrap().insert(key, quote);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn sample_quote(input: &str, output: &str, in_amt: u64, out_amt: u64) -> Quote {
        Quote {
            input_mint: input.into(),
            output_mint: output.into(),
            input_amount: in_amt,
            output_amount: out_amt,
            price_impact_bps: 10,
            route: serde_json::json!({}),
            observed_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn deterministic_quote_returns_seeded_value() {
        let exec = DeterministicExecutor::new()
            .with_quotes(vec![sample_quote("SOL", "TOKEN", 1_000_000, 5_000_000)]);
        let q = exec.quote("SOL", "TOKEN", 1_000_000, 0).await.unwrap();
        assert_eq!(q.output_amount, 5_000_000);
        assert_eq!(q.input_amount, 1_000_000);
    }

    #[tokio::test]
    async fn deterministic_quote_missing_returns_error() {
        let exec = DeterministicExecutor::new();
        assert!(exec.quote("SOL", "TOKEN", 1_000_000, 0).await.is_err());
    }

    #[tokio::test]
    async fn deterministic_execute_returns_quoted_amounts() {
        let exec = DeterministicExecutor::new()
            .with_quotes(vec![sample_quote("SOL", "TOKEN", 1_000_000, 5_000_000)]);
        let q = exec.quote("SOL", "TOKEN", 1_000_000, 0).await.unwrap();
        let fill = exec
            .execute(ExecutionRequest {
                order_id: "test-order".into(),
                quote: q,
                max_slippage_bps: 100,
                max_price_impact_bps: 500,
                min_output_amount: 0,
                input_decimals: 9,
                output_decimals: 6,
                value_basis: super::super::ValueBasis::InputValueUsd(dec!(10)),
            })
            .await
            .unwrap();
        assert_eq!(fill.input_amount, 1_000_000);
        assert_eq!(fill.output_amount, 5_000_000);
        assert!(fill.signature.starts_with("replay:"));
        assert!(!fill.fees_usd.is_sign_negative());
    }

    #[tokio::test]
    async fn health_always_succeeds() {
        let exec = DeterministicExecutor::new();
        assert!(exec.health().await.is_ok());
    }
}
