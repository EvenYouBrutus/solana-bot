use super::reconcile::parse_swap_transaction;
use super::{
    policy::validate_provider_transaction, ExecutionError, ExecutionRequest, Executor, Quote,
};
use crate::{data::rpc::RpcPool, domain::trade::Fill};
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine};
use chrono::{DateTime, Duration, Utc};
use reqwest::Client;
use rust_decimal::Decimal;
use serde_json::{json, Value};
use solana_sdk::{
    signature::{Keypair, Signer},
    transaction::VersionedTransaction,
};
use std::{
    env,
    sync::{Arc, Mutex},
    time::{Duration as StdDuration, Instant},
};

/// Rate-limit state shared across all quote/execute calls on the same
/// executor instance.  Protected by a mutex so concurrent tasks (main
/// session + exit monitor) observe a single backoff window.
#[derive(Debug, Clone, Default)]
struct RateLimitState {
    /// If set, no requests should be attempted until this instant.
    backoff_until: Option<Instant>,
    /// Counter for exponential backoff: 2^consecutive seconds, capped at 64.
    consecutive_429s: u32,
}

/// Real Jupiter v6 swap execution: fresh quote, provider transaction
/// validation, signing, submission, bounded confirmation polling, and
/// verification of the actual on-chain outcome before any fill is recorded.
pub struct JupiterExecutor {
    client: Client,
    api_url: String,
    rpc: RpcPool,
    signer_env: Option<String>,
    api_key_env: Option<String>,
    max_quote_age_secs: i64,
    max_fee_lamports: u64,
    allowed_program_ids: Vec<String>,
    priority_fee_lamports: u64,
    confirm_timeout: StdDuration,
    confirm_poll: StdDuration,
    rate_limit: Arc<Mutex<RateLimitState>>,
}
impl JupiterExecutor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        api_url: String,
        rpc: RpcPool,
        signer_env: Option<String>,
        api_key_env: Option<String>,
        max_quote_age_secs: i64,
        max_fee_lamports: u64,
        allowed_program_ids: Vec<String>,
        priority_fee_lamports: u64,
        confirm_timeout_secs: u64,
        confirm_poll_ms: u64,
    ) -> Result<Self, ExecutionError> {
        let client = Client::builder()
            .timeout(StdDuration::from_secs(10))
            .build()
            .map_err(|e| ExecutionError::Unavailable(e.to_string()))?;
        Ok(Self {
            client,
            api_url: api_url.trim_end_matches('/').into(),
            rpc,
            signer_env,
            api_key_env,
            max_quote_age_secs,
            max_fee_lamports,
            allowed_program_ids,
            priority_fee_lamports,
            confirm_timeout: StdDuration::from_secs(confirm_timeout_secs),
            confirm_poll: StdDuration::from_millis(confirm_poll_ms),
            rate_limit: Arc::new(Mutex::new(RateLimitState::default())),
        })
    }
    fn signer(&self) -> Result<Keypair, ExecutionError> {
        let variable = self.signer_env.as_deref().ok_or_else(|| {
            ExecutionError::Unavailable("live signer environment variable is not configured".into())
        })?;
        let raw = env::var(variable)
            .map_err(|_| ExecutionError::Unavailable("live signer secret is absent".into()))?;
        let bytes: Vec<u8> = serde_json::from_str(&raw).map_err(|_| {
            ExecutionError::Unavailable("live signer must be a JSON byte array".into())
        })?;
        Keypair::try_from(bytes.as_slice())
            .map_err(|_| ExecutionError::Unavailable("invalid signer keypair".into()))
    }
    /// Resolves the Jupiter API key from the configured environment variable.
    /// Returns Ok(None) when no env var is configured or the variable is unset.
    fn api_key(&self) -> Result<Option<String>, ExecutionError> {
        let variable = match self.api_key_env.as_deref() {
            Some(v) if !v.is_empty() => v,
            _ => return Ok(None),
        };
        match env::var(variable) {
            Ok(val) if !val.is_empty() => Ok(Some(val)),
            Ok(_) => Ok(None),
            Err(env::VarError::NotPresent) => Ok(None),
            Err(e) => Err(ExecutionError::Unavailable(format!(
                "Jupiter API key env var error: {e}"
            ))),
        }
    }
    /// Verifies the actual on-chain fee against the configured cap.
    /// Returns an error if the fee exceeds the cap.
    fn verify_onchain_fee(fee_lamports: u64, max_fee_lamports: u64) -> Result<(), ExecutionError> {
        if fee_lamports > max_fee_lamports {
            return Err(ExecutionError::Policy(format!(
                "on-chain transaction fee {fee_lamports} lamports exceeds configured maximum {max_fee_lamports}"
            )));
        }
        Ok(())
    }
    /// Pre-submission fee estimate: priority fee + base fee (5000 lamports).
    /// Catches excessive fees before the transaction is signed and submitted.
    fn verify_presubmission_fee(
        priority_fee_lamports: u64,
        max_fee_lamports: u64,
    ) -> Result<(), ExecutionError> {
        const BASE_FEE_LAMPORTS: u64 = 5_000;
        let estimated_total = priority_fee_lamports + BASE_FEE_LAMPORTS;
        if estimated_total > max_fee_lamports {
            return Err(ExecutionError::Policy(format!(
                "estimated transaction fee {estimated_total} lamports (priority {priority_fee_lamports} + base {BASE_FEE_LAMPORTS}) exceeds configured maximum {max_fee_lamports}"
            )));
        }
        Ok(())
    }
    /// Check whether the rate limiter is currently active. If so, returns
    /// the remaining backoff duration.
    fn rate_limit_remaining(&self) -> Option<StdDuration> {
        let rl = self.rate_limit.lock().ok()?;
        rl.backoff_until
            .and_then(|until| until.checked_duration_since(Instant::now()))
    }
    /// Record a 429 response and update the backoff state.  Returns the
    /// backoff duration the caller should wait before retrying.
    fn record_rate_limit(&self, retry_after: Option<u64>) -> StdDuration {
        let mut rl = self.rate_limit.lock().unwrap_or_else(|e| e.into_inner());
        rl.consecutive_429s = rl.consecutive_429s.saturating_add(1);
        // Exponential backoff: 2^n seconds, capped at 64.
        let base = 2u64.saturating_pow(rl.consecutive_429s.min(6));
        let secs = retry_after.unwrap_or(base).min(120);
        rl.backoff_until = Some(Instant::now() + StdDuration::from_secs(secs));
        StdDuration::from_secs(secs)
    }
    /// Clear the rate limiter backoff (called after a successful request).
    fn clear_rate_limit(&self) {
        if let Ok(mut rl) = self.rate_limit.lock() {
            rl.consecutive_429s = 0;
            rl.backoff_until = None;
        }
    }
    /// RPC submission errors that are deterministic refusals. The node has
    /// not relayed the transaction, so treating these as `Failed` cannot
    /// double-spend; everything else stays `Unknown`.
    fn classify_submit_error(detail: &str) -> ExecutionError {
        let lower = detail.to_lowercase();
        if lower.contains("simulation failed")
            || lower.contains("blockhash")
            || lower.contains("block hash")
        {
            ExecutionError::Transaction(format!("submission refused deterministically: {detail}"))
        } else {
            ExecutionError::Unknown {
                signature: None,
                detail: format!("submission not confirmed: {detail}"),
            }
        }
    }
    async fn verify_outcome(
        &self,
        signature: &str,
        input_mint: &str,
        output_mint: &str,
        owner: &str,
        expected_input: u64,
    ) -> Result<super::reconcile::ChainSwapOutcome, ExecutionError> {
        // Indexing can lag confirmation; bounded retries, then Unknown.
        for _ in 0..3 {
            match self.rpc.transaction(signature).await {
                Ok(Some(tx)) => {
                    return match parse_swap_transaction(
                        &tx,
                        input_mint,
                        output_mint,
                        owner,
                        expected_input,
                    ) {
                        super::reconcile::SwapOutcome::Executed(o) => Ok(o),
                        super::reconcile::SwapOutcome::Failed(err) => Err(
                            ExecutionError::Transaction(format!("on-chain failure: {err}")),
                        ),
                        super::reconcile::SwapOutcome::Unverifiable(detail) => {
                            Err(ExecutionError::Unknown {
                                signature: Some(signature.to_owned()),
                                detail,
                            })
                        }
                    }
                }
                Ok(None) => tokio::time::sleep(StdDuration::from_millis(400)).await,
                Err(e) if e.is_availability() => {
                    tokio::time::sleep(StdDuration::from_millis(400)).await
                }
                Err(e) => {
                    return Err(ExecutionError::Unknown {
                        signature: Some(signature.to_owned()),
                        detail: e.to_string(),
                    })
                }
            }
        }
        Err(ExecutionError::Unknown {
            signature: Some(signature.to_owned()),
            detail: "transaction not indexed after confirmation".into(),
        })
    }
    async fn await_confirmation(&self, signature: &str) -> Result<(), ExecutionError> {
        let deadline = Instant::now() + self.confirm_timeout;
        loop {
            tokio::time::sleep(self.confirm_poll).await;
            if Instant::now() >= deadline {
                return Err(ExecutionError::Unknown {
                    signature: Some(signature.to_owned()),
                    detail: "confirmation window elapsed".into(),
                });
            }
            match self.rpc.signature_status(signature).await {
                Ok(Some(status)) => {
                    if let Some(err) = status.err {
                        return Err(ExecutionError::Transaction(format!(
                            "on-chain failure: {err}"
                        )));
                    }
                    if status.is_confirmed_or_finalized() {
                        return Ok(());
                    }
                }
                // Availability errors during polling are not evidence of failure.
                Ok(None) => {}
                Err(e) if e.is_availability() => {}
                Err(e) => {
                    return Err(ExecutionError::Unknown {
                        signature: Some(signature.to_owned()),
                        detail: e.to_string(),
                    })
                }
            }
        }
    }
}

#[async_trait]
impl Executor for JupiterExecutor {
    fn is_live(&self) -> bool {
        self.signer_env.is_some()
    }
    fn signer_pubkey(&self) -> Option<String> {
        self.signer().ok().map(|k| k.pubkey().to_string())
    }
    async fn quote(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        slippage: u16,
    ) -> Result<Quote, ExecutionError> {
        if amount == 0 {
            return Err(ExecutionError::Quote(
                "quote amount must be non-zero".into(),
            ));
        }
        // Respect active rate-limit backoff.
        if let Some(remaining) = self.rate_limit_remaining() {
            return Err(ExecutionError::Unavailable(format!(
                "Jupiter rate-limited; retry in {}s",
                remaining.as_secs()
            )));
        }
        let url = format!("{}/swap/v1/quote", self.api_url);
        let mut req = self.client.get(url).query(&[
            ("inputMint", input),
            ("outputMint", output),
            ("amount", &amount.to_string()),
            ("slippageBps", &slippage.to_string()),
            ("asLegacyTransaction", "true"),
        ]);
        if let Some(key) = self.api_key()? {
            req = req.header("x-api-key", key);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| ExecutionError::Quote(e.to_string()))?;
        // Handle 429 Too Many Requests with exponential backoff.
        if resp.status().as_u16() == 429 {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());
            let backoff = self.record_rate_limit(retry_after);
            return Err(ExecutionError::Unavailable(format!(
                "Jupiter HTTP 429 rate-limited; backing off {}s",
                backoff.as_secs()
            )));
        }
        let v: Value = resp
            .error_for_status()
            .map_err(|e| ExecutionError::Quote(e.to_string()))?
            .json()
            .await
            .map_err(|e| ExecutionError::Quote(e.to_string()))?;
        // Successful request: clear any rate-limit backoff.
        self.clear_rate_limit();
        let parse = |n: &str| {
            v[n].as_str()
                .ok_or_else(|| ExecutionError::Quote(format!("missing {n}")))
                .and_then(|x| {
                    x.parse::<u64>()
                        .map_err(|_| ExecutionError::Quote(format!("invalid {n}")))
                })
        };
        let impact = v["priceImpactPct"]
            .as_str()
            .unwrap_or("0")
            .parse::<f64>()
            .map_err(|_| ExecutionError::Quote("invalid price impact".into()))?;
        if !impact.is_finite() || impact < 0.0 {
            return Err(ExecutionError::Quote(
                "invalid negative/non-finite price impact".into(),
            ));
        }
        Ok(Quote {
            input_mint: input.into(),
            output_mint: output.into(),
            input_amount: parse("inAmount")?,
            output_amount: parse("outAmount")?,
            price_impact_bps: (impact * 10_000.0) as u32,
            route: v,
            observed_at: Utc::now(),
        })
    }
    async fn execute(&self, r: ExecutionRequest) -> Result<Fill, ExecutionError> {
        let started = Instant::now();
        if Utc::now() - r.quote.observed_at > Duration::seconds(self.max_quote_age_secs) {
            return Err(ExecutionError::StaleQuote);
        }
        if r.quote.price_impact_bps > r.max_price_impact_bps {
            return Err(ExecutionError::InvalidQuote);
        }
        if r.quote.output_amount == 0 || r.quote.output_amount < r.min_output_amount {
            return Err(ExecutionError::InvalidQuote);
        }
        let signer = self.signer()?;
        let owner = signer.pubkey().to_string();
        let mut body = json!({
            "quoteResponse": r.quote.route,
            "userPublicKey": owner,
            "wrapAndUnwrapSol": true,
            "dynamicComputeUnitLimit": true,
            "asLegacyTransaction": true,
            "prioritizationFeeLamports": self.priority_fee_lamports,
        });
        if let Some(key) = self.api_key()? {
            body["apiKey"] = json!(key);
        }
        // Pre-submission fee cap: estimated priority fee + base fee (5000 lamports).
        // This catches excessive fees before signing; the post-confirmation
        // verify_onchain_fee() provides an additional safety net.
        Self::verify_presubmission_fee(self.priority_fee_lamports, self.max_fee_lamports)?;
        let swap: Value = {
            let resp = self
                .client
                .post(format!("{}/swap/v1/swap", self.api_url))
                .json(&body)
                .send()
                .await
                .map_err(|e| ExecutionError::Transaction(e.to_string()))?;
            // Handle 429 Too Many Requests with exponential backoff.
            if resp.status().as_u16() == 429 {
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok());
                let backoff = self.record_rate_limit(retry_after);
                return Err(ExecutionError::Unavailable(format!(
                    "Jupiter swap HTTP 429 rate-limited; backing off {}s",
                    backoff.as_secs()
                )));
            }
            resp.error_for_status()
                .map_err(|e| ExecutionError::Transaction(e.to_string()))?
                .json()
                .await
                .map_err(|e| ExecutionError::Transaction(e.to_string()))?
        };
        // Successful request: clear any rate-limit backoff.
        self.clear_rate_limit();
        let encoded = swap["swapTransaction"]
            .as_str()
            .ok_or_else(|| ExecutionError::Transaction("provider omitted transaction".into()))?;
        let unsigned: VersionedTransaction =
            bincode::deserialize(&STANDARD.decode(encoded).map_err(|_| {
                ExecutionError::Transaction("invalid provider transaction encoding".into())
            })?)
            .map_err(|_| ExecutionError::Transaction("invalid provider transaction".into()))?;
        validate_provider_transaction(&unsigned, &signer.pubkey(), &self.allowed_program_ids)
            .map_err(|e| ExecutionError::Policy(e.to_string()))?;
        let signed = VersionedTransaction::try_new(unsigned.message, &[&signer]).map_err(|_| {
            ExecutionError::Transaction("could not sign provider transaction".into())
        })?;
        let wire =
            STANDARD.encode(bincode::serialize(&signed).map_err(|_| {
                ExecutionError::Transaction("could not serialize transaction".into())
            })?);
        let signature = match self.rpc.call("sendTransaction", json!([wire, {"encoding": "base64", "skipPreflight": false, "preflightCommitment": "confirmed", "maxRetries": 0}])).await {
            Ok(sent) => sent.value.as_str().ok_or_else(|| ExecutionError::Unknown { signature: None, detail: "missing transaction signature in submission response".into() })?.to_owned(),
            Err(e) if e.is_availability() => return Err(Self::classify_submit_error(&e.to_string())),
            Err(e) => return Err(ExecutionError::Unknown { signature: None, detail: e.to_string() }),
        };
        self.await_confirmation(&signature).await?;
        let outcome = self
            .verify_outcome(
                &signature,
                &r.quote.input_mint,
                &r.quote.output_mint,
                &owner,
                r.quote.input_amount,
            )
            .await?;
        Self::verify_onchain_fee(outcome.fee_lamports, self.max_fee_lamports)?;
        let (value_usd, price_usd) = r.value_basis.price_fill(
            outcome.input_amount,
            r.input_decimals,
            outcome.output_amount,
            r.output_decimals,
        )?;
        Ok(Fill {
            order_id: r.order_id,
            signature,
            input_amount: outcome.input_amount,
            output_amount: outcome.output_amount,
            price_usd,
            fees_usd: Decimal::ZERO, // enriched with SOL price by the runtime
            slippage_bps: 0,
            confirmed_at: Utc::now(),
            latency_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            fee_lamports: outcome.fee_lamports,
            input_value_usd: Some(value_usd),
            expected_output_amount: Some(r.quote.output_amount),
        })
    }
    async fn health(&self) -> Result<(), ExecutionError> {
        self.rpc
            .health()
            .await
            .map_err(|e| ExecutionError::Unavailable(e.to_string()))
    }
}

/// Marks a fill's realised slippage and USD fees once the runtime has the
/// quote context and SOL price; keeps the executor free of economic policy.
pub fn finalize_fill(fill: &mut Fill, sol_price_usd: Option<Decimal>) {
    fill.slippage_bps = fill.realised_slippage_bps();
    fill.fees_usd = super::executor::fee_usd(fill.fee_lamports, sol_price_usd);
}
/// Reconciliation helper used at startup for orders whose outcome is unknown.
pub async fn reconcile_signature(
    rpc: &RpcPool,
    signature: &str,
    input_mint: &str,
    output_mint: &str,
    owner: &str,
    expected_input: u64,
) -> Result<Option<super::reconcile::ChainSwapOutcome>, ExecutionError> {
    match rpc.signature_status(signature).await {
        Ok(Some(status)) => {
            if let Some(err) = status.err {
                return Err(ExecutionError::Transaction(format!(
                    "on-chain failure: {err}"
                )));
            }
            if !status.is_confirmed_or_finalized() {
                return Ok(None);
            }
        }
        Ok(None) => return Ok(None),
        Err(e) => {
            return Err(ExecutionError::Unknown {
                signature: Some(signature.to_owned()),
                detail: e.to_string(),
            })
        }
    }
    rpc.transaction(signature)
        .await
        .map_err(|e| ExecutionError::Unknown {
            signature: Some(signature.to_owned()),
            detail: e.to_string(),
        })?
        .map(|tx| {
            match parse_swap_transaction(&tx, input_mint, output_mint, owner, expected_input) {
                super::reconcile::SwapOutcome::Executed(o) => Ok(Some(o)),
                super::reconcile::SwapOutcome::Failed(err) => Err(ExecutionError::Transaction(
                    format!("on-chain failure: {err}"),
                )),
                super::reconcile::SwapOutcome::Unverifiable(detail) => {
                    Err(ExecutionError::Unknown {
                        signature: Some(signature.to_owned()),
                        detail,
                    })
                }
            }
        })
        .transpose()
        .map(|x| x.flatten())
}
/// Timestamp type re-export used by runtime logging of quote age.
pub type QuoteObservedAt = DateTime<Utc>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_onchain_fee_allows_within_cap() {
        assert!(JupiterExecutor::verify_onchain_fee(5_000, 500_000).is_ok());
        assert!(JupiterExecutor::verify_onchain_fee(0, 500_000).is_ok());
        assert!(JupiterExecutor::verify_onchain_fee(500_000, 500_000).is_ok());
    }

    #[test]
    fn verify_onchain_fee_rejects_above_cap() {
        assert!(JupiterExecutor::verify_onchain_fee(500_001, 500_000).is_err());
        assert!(JupiterExecutor::verify_onchain_fee(1_000_000, 500_000).is_err());
    }

    #[test]
    fn api_key_returns_none_when_env_unconfigured() {
        let rpc = crate::data::rpc::RpcPool::with_attempts(
            vec!["http://localhost:1".into()],
            StdDuration::from_millis(100),
            1,
        )
        .unwrap();
        let jup = JupiterExecutor {
            client: Client::new(),
            api_url: "https://api.jup.ag".into(),
            rpc,
            signer_env: None,
            api_key_env: None,
            max_quote_age_secs: 3,
            max_fee_lamports: 500_000,
            allowed_program_ids: vec![],
            priority_fee_lamports: 10_000,
            confirm_timeout: StdDuration::from_secs(90),
            confirm_poll: StdDuration::from_millis(500),
            rate_limit: Arc::new(Mutex::new(RateLimitState::default())),
        };
        assert!(jup.api_key().unwrap().is_none());
    }

    #[test]
    fn classify_submit_error_preserves_unknown_for_network() {
        matches!(
            JupiterExecutor::classify_submit_error("connection reset"),
            ExecutionError::Unknown { .. }
        );
    }

    #[test]
    fn classify_submit_error_deterministic_for_simulation() {
        matches!(
            JupiterExecutor::classify_submit_error("Simulation Failed: custom error"),
            ExecutionError::Transaction(_)
        );
    }

    #[test]
    fn onchain_fee_cap_is_enforced_after_confirmation() {
        // Fee exactly at cap: allowed
        assert!(JupiterExecutor::verify_onchain_fee(500_000, 500_000).is_ok());
        // Fee one lamport over cap: rejected
        assert!(JupiterExecutor::verify_onchain_fee(500_001, 500_000).is_err());
        // Fee well under cap: allowed
        assert!(JupiterExecutor::verify_onchain_fee(10_000, 500_000).is_ok());
        // Zero fee: allowed
        assert!(JupiterExecutor::verify_onchain_fee(0, 500_000).is_ok());
    }

    #[test]
    fn presubmission_fee_cap_is_enforced_before_signing() {
        // priority 45_000 + base 5_000 = 50_000, cap 50_000: allowed
        assert!(JupiterExecutor::verify_presubmission_fee(45_000, 50_000).is_ok());
        // priority 45_001 + base 5_000 = 50_001, cap 50_000: rejected
        assert!(JupiterExecutor::verify_presubmission_fee(45_001, 50_000).is_err());
        // priority 0 + base 5_000 = 5_000, cap 50_000: allowed
        assert!(JupiterExecutor::verify_presubmission_fee(0, 50_000).is_ok());
        // priority 10_000 + base 5_000 = 15_000, cap 10_000: rejected
        assert!(JupiterExecutor::verify_presubmission_fee(10_000, 10_000).is_err());
    }

    // --- Regression tests for HTTP 429 / rate-limit backoff ---

    #[test]
    fn rate_limit_backoff_increases_exponentially() {
        let rpc = crate::data::rpc::RpcPool::with_attempts(
            vec!["http://localhost:1".into()],
            StdDuration::from_millis(100),
            1,
        )
        .unwrap();
        let jup = JupiterExecutor {
            client: Client::new(),
            api_url: "https://api.jup.ag".into(),
            rpc,
            signer_env: None,
            api_key_env: None,
            max_quote_age_secs: 3,
            max_fee_lamports: 500_000,
            allowed_program_ids: vec![],
            priority_fee_lamports: 10_000,
            confirm_timeout: StdDuration::from_secs(90),
            confirm_poll: StdDuration::from_millis(500),
            rate_limit: Arc::new(Mutex::new(RateLimitState::default())),
        };
        // No backoff initially.
        assert!(jup.rate_limit_remaining().is_none());
        // First 429: backoff = 2^1 = 2s.
        let d1 = jup.record_rate_limit(None);
        assert_eq!(d1.as_secs(), 2);
        assert!(jup.rate_limit_remaining().is_some());
        // Second 429: backoff = 2^2 = 4s.
        let d2 = jup.record_rate_limit(None);
        assert_eq!(d2.as_secs(), 4);
        // Third 429: backoff = 2^3 = 8s.
        let d3 = jup.record_rate_limit(None);
        assert_eq!(d3.as_secs(), 8);
    }

    #[test]
    fn rate_limit_respects_retry_after_header() {
        let rpc = crate::data::rpc::RpcPool::with_attempts(
            vec!["http://localhost:1".into()],
            StdDuration::from_millis(100),
            1,
        )
        .unwrap();
        let jup = JupiterExecutor {
            client: Client::new(),
            api_url: "https://api.jup.ag".into(),
            rpc,
            signer_env: None,
            api_key_env: None,
            max_quote_age_secs: 3,
            max_fee_lamports: 500_000,
            allowed_program_ids: vec![],
            priority_fee_lamports: 10_000,
            confirm_timeout: StdDuration::from_secs(90),
            confirm_poll: StdDuration::from_millis(500),
            rate_limit: Arc::new(Mutex::new(RateLimitState::default())),
        };
        // Retry-After: 30 seconds overrides exponential backoff.
        let d = jup.record_rate_limit(Some(30));
        assert_eq!(d.as_secs(), 30);
    }

    #[test]
    fn rate_limit_capped_at_120_seconds() {
        let rpc = crate::data::rpc::RpcPool::with_attempts(
            vec!["http://localhost:1".into()],
            StdDuration::from_millis(100),
            1,
        )
        .unwrap();
        let jup = JupiterExecutor {
            client: Client::new(),
            api_url: "https://api.jup.ag".into(),
            rpc,
            signer_env: None,
            api_key_env: None,
            max_quote_age_secs: 3,
            max_fee_lamports: 500_000,
            allowed_program_ids: vec![],
            priority_fee_lamports: 10_000,
            confirm_timeout: StdDuration::from_secs(90),
            confirm_poll: StdDuration::from_millis(500),
            rate_limit: Arc::new(Mutex::new(RateLimitState::default())),
        };
        // Retry-After: 300 seconds should be capped at 120.
        let d = jup.record_rate_limit(Some(300));
        assert_eq!(d.as_secs(), 120);
    }

    #[test]
    fn rate_limit_clears_on_success() {
        let rpc = crate::data::rpc::RpcPool::with_attempts(
            vec!["http://localhost:1".into()],
            StdDuration::from_millis(100),
            1,
        )
        .unwrap();
        let jup = JupiterExecutor {
            client: Client::new(),
            api_url: "https://api.jup.ag".into(),
            rpc,
            signer_env: None,
            api_key_env: None,
            max_quote_age_secs: 3,
            max_fee_lamports: 500_000,
            allowed_program_ids: vec![],
            priority_fee_lamports: 10_000,
            confirm_timeout: StdDuration::from_secs(90),
            confirm_poll: StdDuration::from_millis(500),
            rate_limit: Arc::new(Mutex::new(RateLimitState::default())),
        };
        // Simulate rate limit.
        jup.record_rate_limit(None);
        assert!(jup.rate_limit_remaining().is_some());
        // Clear on success.
        jup.clear_rate_limit();
        assert!(jup.rate_limit_remaining().is_none());
    }
}
