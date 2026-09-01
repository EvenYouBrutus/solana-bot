use super::{ExecutionError, ExecutionRequest, Executor, Quote};
use crate::domain::trade::Fill;
use async_trait::async_trait;
use chrono::Utc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Paper mode shares quotes, policy gates, and pricing with live mode; only
/// the signing/submission is replaced by a realistic simulated fill with
/// slippage, fees, price impact, and occasional failed transactions.
pub struct PaperExecutor<E> {
    quotes: E,
    fill_haircut_bps: u32,
    /// Simulated base fee in lamports (5000).
    base_fee_lamports: u64,
    /// Simulated priority fee in lamports.
    priority_fee_lamports: u64,
    /// Counter for deterministic failure injection (every nth call fails).
    call_counter: AtomicU64,
    /// Fail every N calls (0 = no failures).
    fail_every_n: u64,
}
impl<E> PaperExecutor<E> {
    pub fn new(quotes: E, fill_haircut_bps: u32) -> Self {
        Self {
            quotes,
            fill_haircut_bps,
            base_fee_lamports: 5_000,
            priority_fee_lamports: 10_000,
            call_counter: AtomicU64::new(0),
            fail_every_n: 0,
        }
    }

    pub fn with_failure_injection(quotes: E, fill_haircut_bps: u32, fail_every_n: u64) -> Self {
        Self {
            quotes,
            fill_haircut_bps,
            base_fee_lamports: 5_000,
            priority_fee_lamports: 10_000,
            call_counter: AtomicU64::new(0),
            fail_every_n,
        }
    }
}

#[async_trait]
impl<E: Executor> Executor for PaperExecutor<E> {
    fn is_live(&self) -> bool {
        false
    }
    fn signer_pubkey(&self) -> Option<String> {
        None
    }
    async fn quote(&self, a: &str, b: &str, c: u64, d: u16) -> Result<Quote, ExecutionError> {
        self.quotes.quote(a, b, c, d).await
    }
    async fn execute(&self, r: ExecutionRequest) -> Result<Fill, ExecutionError> {
        let age = chrono::Utc::now() - r.quote.observed_at;
        if age < chrono::Duration::zero() || age > chrono::Duration::seconds(300) {
            return Err(ExecutionError::StaleQuote);
        }
        if r.quote.output_amount == 0
            || r.quote.price_impact_bps > r.max_price_impact_bps
            || r.quote.output_amount < r.min_output_amount
        {
            return Err(ExecutionError::InvalidQuote);
        }

        // Simulated failure injection.
        if self.fail_every_n > 0 {
            let count = self.call_counter.fetch_add(1, Ordering::SeqCst) + 1;
            if count.is_multiple_of(self.fail_every_n) {
                return Err(ExecutionError::Transaction(
                    "paper: simulated transaction failure".into(),
                ));
            }
        }

        // Apply haircut (adversarial fill worse than quote).
        let haircut = 10_000u64.saturating_sub(self.fill_haircut_bps as u64);
        let output = r.quote.output_amount.saturating_mul(haircut) / 10_000;
        if output < r.min_output_amount {
            return Err(ExecutionError::InvalidQuote);
        }

        // Simulated slippage: output is slightly worse than quote.
        let slippage_bps = r.quote.price_impact_bps / 3 + 5;
        let after_slippage = output.saturating_mul(10_000 - slippage_bps as u64) / 10_000;
        if after_slippage < r.min_output_amount {
            return Err(ExecutionError::InvalidQuote);
        }

        let (value_usd, price_usd) = r.value_basis.price_fill(
            r.quote.input_amount,
            r.input_decimals,
            after_slippage,
            r.output_decimals,
        )?;

        // Simulated fees: base fee + priority fee.
        let fee_lamports = self.base_fee_lamports + self.priority_fee_lamports;
        let fees_usd = super::executor::fee_usd(fee_lamports, None);

        Ok(Fill {
            order_id: r.order_id,
            signature: format!("paper:{}", uuid::Uuid::new_v4()),
            input_amount: r.quote.input_amount,
            output_amount: after_slippage,
            price_usd,
            fees_usd,
            slippage_bps,
            confirmed_at: Utc::now(),
            latency_ms: 0,
            fee_lamports,
            input_value_usd: Some(value_usd),
            expected_output_amount: Some(r.quote.output_amount),
        })
    }
    async fn health(&self) -> Result<(), ExecutionError> {
        self.quotes.health().await
    }
    fn register_quote(&self, quote: Quote) {
        self.quotes.register_quote(quote);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::executor::ValueBasis;
    use rust_decimal_macros::dec;
    use std::time::Duration;

    struct FakeExecutor {
        fail: bool,
    }
    #[async_trait::async_trait]
    impl Executor for FakeExecutor {
        async fn quote(
            &self,
            _a: &str,
            _b: &str,
            amount: u64,
            _s: u16,
        ) -> Result<Quote, ExecutionError> {
            if self.fail {
                return Err(ExecutionError::Quote("down".into()));
            }
            Ok(Quote {
                input_mint: "SOL".into(),
                output_mint: "T".into(),
                input_amount: amount,
                output_amount: 10_000_000,
                price_impact_bps: 10,
                route: serde_json::json!({}),
                observed_at: Utc::now(),
            })
        }
        async fn execute(&self, _r: ExecutionRequest) -> Result<Fill, ExecutionError> {
            Err(ExecutionError::Unavailable("paper never delegates".into()))
        }
        async fn health(&self) -> Result<(), ExecutionError> {
            Ok(())
        }
    }
    fn request() -> ExecutionRequest {
        let quote = Quote {
            input_mint: "SOL".into(),
            output_mint: "T".into(),
            input_amount: 1_000_000,
            output_amount: 10_000_000,
            price_impact_bps: 10,
            route: serde_json::json!({}),
            observed_at: Utc::now(),
        };
        ExecutionRequest {
            order_id: "o".into(),
            quote,
            max_slippage_bps: 500,
            max_price_impact_bps: 300,
            min_output_amount: 5_000_000,
            input_decimals: 9,
            output_decimals: 6,
            value_basis: ValueBasis::InputValueUsd(dec!(10)),
        }
    }
    #[tokio::test]
    async fn paper_fill_applies_haircut_slippage_and_fees() {
        let paper = PaperExecutor::new(FakeExecutor { fail: false }, 100);
        let fill = paper.execute(request()).await.unwrap();
        // Haircut: 10_000_000 * 9900/10000 = 9_900_000
        // Slippage (price_impact 10/3 + 5 = 8 bps): 9_900_000 * 9992/10000 = 9_892_080
        assert_eq!(fill.output_amount, 9_892_080);
        assert!(fill.slippage_bps > 0, "should have simulated slippage");
        assert!(fill.fee_lamports > 0, "should have simulated fees");
        assert!(fill.signature.starts_with("paper:"));
        assert!(!paper.is_live());
        assert!(paper.signer_pubkey().is_none());
    }
    #[tokio::test]
    async fn paper_rejects_excessive_price_impact() {
        let paper = PaperExecutor::new(FakeExecutor { fail: false }, 0);
        let mut r = request();
        r.quote.price_impact_bps = 400;
        assert!(matches!(
            paper.execute(r).await,
            Err(ExecutionError::InvalidQuote)
        ));
    }
    #[tokio::test]
    async fn paper_quote_passthrough_and_health() {
        let paper = PaperExecutor::new(FakeExecutor { fail: true }, 25);
        assert!(paper.quote("a", "b", 1, 10).await.is_err());
        assert!(paper.health().await.is_ok());
    }
    #[test]
    fn rpc_pool_validates_config() {
        assert!(crate::data::rpc::RpcPool::with_attempts(
            vec!["http://localhost:1".into()],
            Duration::from_millis(100),
            0
        )
        .is_ok());
    }

    struct PanicExecutor;
    #[async_trait::async_trait]
    impl Executor for PanicExecutor {
        async fn quote(
            &self,
            _a: &str,
            _b: &str,
            _amount: u64,
            _s: u16,
        ) -> Result<Quote, ExecutionError> {
            Ok(Quote {
                input_mint: "SOL".into(),
                output_mint: "T".into(),
                input_amount: 1_000_000,
                output_amount: 10_000_000,
                price_impact_bps: 10,
                route: serde_json::json!({}),
                observed_at: Utc::now(),
            })
        }
        async fn execute(&self, _r: ExecutionRequest) -> Result<Fill, ExecutionError> {
            panic!("PaperExecutor must never delegate execute to inner executor")
        }
        async fn health(&self) -> Result<(), ExecutionError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn paper_execute_never_delegates_to_inner_executor() {
        let paper = PaperExecutor::new(PanicExecutor, 25);
        let r = request();
        let fill = paper.execute(r).await.expect("paper execute must succeed");
        assert!(fill.signature.starts_with("paper:"));
        assert!(!paper.is_live());
        assert!(paper.signer_pubkey().is_none());
    }

    #[tokio::test]
    async fn paper_rejects_stale_quote() {
        let paper = PaperExecutor::new(FakeExecutor { fail: false }, 25);
        let mut r = request();
        r.quote.observed_at = Utc::now() - chrono::Duration::seconds(600);
        assert!(matches!(
            paper.execute(r).await,
            Err(ExecutionError::StaleQuote)
        ));
    }

    #[tokio::test]
    async fn paper_rejects_zero_output() {
        let paper = PaperExecutor::new(FakeExecutor { fail: false }, 0);
        let mut r = request();
        r.quote.output_amount = 0;
        assert!(matches!(
            paper.execute(r).await,
            Err(ExecutionError::InvalidQuote)
        ));
    }

    #[tokio::test]
    async fn paper_failure_injection_triggers_errors() {
        let paper = PaperExecutor::with_failure_injection(FakeExecutor { fail: false }, 25, 3);
        let r = request();
        // Call 3 times: 1st ok, 2nd ok, 3rd fails.
        let _ = paper.execute(r.clone()).await;
        let _ = paper.execute(r.clone()).await;
        let result = paper.execute(r).await;
        assert!(
            matches!(result, Err(ExecutionError::Transaction(_))),
            "every 3rd call should fail"
        );
    }
}
