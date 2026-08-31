use super::reconcile::parse_swap_transaction;
use super::{
    policy::validate_provider_transaction, ExecutionError, ExecutionRequest, Executor, Quote,
    ValueBasis,
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
    time::{Duration as StdDuration, Instant},
};

/// Real Jupiter v6 swap execution: fresh quote, provider transaction
/// validation, signing, submission, bounded confirmation polling, and
/// verification of the actual on-chain outcome before any fill is recorded.
pub struct JupiterExecutor {
    client: Client,
    api_url: String,
    rpc: RpcPool,
    signer_env: Option<String>,
    max_quote_age_secs: i64,
    allowed_program_ids: Vec<String>,
    priority_fee_lamports: u64,
    confirm_timeout: StdDuration,
    confirm_poll: StdDuration,
}
impl JupiterExecutor {
    pub fn new(
        api_url: String,
        rpc: RpcPool,
        signer_env: Option<String>,
        max_quote_age_secs: i64,
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
            max_quote_age_secs,
            allowed_program_ids,
            priority_fee_lamports,
            confirm_timeout: StdDuration::from_secs(confirm_timeout_secs),
            confirm_poll: StdDuration::from_millis(confirm_poll_ms),
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
        Keypair::from_bytes(&bytes)
            .map_err(|_| ExecutionError::Unavailable("invalid signer keypair".into()))
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
        let url = format!("{}/swap/v1/quote", self.api_url);
        let v: Value = self
            .client
            .get(url)
            .query(&[
                ("inputMint", input),
                ("outputMint", output),
                ("amount", &amount.to_string()),
                ("slippageBps", &slippage.to_string()),
                ("asLegacyTransaction", "true"),
            ])
            .send()
            .await
            .map_err(|e| ExecutionError::Quote(e.to_string()))?
            .error_for_status()
            .map_err(|e| ExecutionError::Quote(e.to_string()))?
            .json()
            .await
            .map_err(|e| ExecutionError::Quote(e.to_string()))?;
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
        if r.quote.output_amount < r.min_output_amount {
            return Err(ExecutionError::InvalidQuote);
        }
        let signer = self.signer()?;
        let owner = signer.pubkey().to_string();
        let body = json!({
            "quoteResponse": r.quote.route,
            "userPublicKey": owner,
            "wrapAndUnwrapSol": true,
            "dynamicComputeUnitLimit": true,
            "asLegacyTransaction": true,
            "prioritizationFeeLamports": self.priority_fee_lamports,
        });
        let swap: Value = self
            .client
            .post(format!("{}/swap/v1/swap", self.api_url))
            .json(&body)
            .send()
            .await
            .map_err(|e| ExecutionError::Transaction(e.to_string()))?
            .error_for_status()
            .map_err(|e| ExecutionError::Transaction(e.to_string()))?
            .json()
            .await
            .map_err(|e| ExecutionError::Transaction(e.to_string()))?;
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
