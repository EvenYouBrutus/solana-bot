use chrono::{DateTime, Utc};
use reqwest::Client;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

/// Bounded exponential backoff schedule for HTTP 429 responses:
/// 350ms → 700ms → 1.4s → 2.8s → 5s max. Never grows beyond the cap, so a
/// rate-limited endpoint can delay a call by at most a few seconds.
const RATE_LIMIT_BACKOFF_START_MS: u64 = 350;
const RATE_LIMIT_BACKOFF_MAX_MS: u64 = 5_000;

fn rate_limit_backoff(step: u32) -> Duration {
    let ms = RATE_LIMIT_BACKOFF_START_MS
        .saturating_mul(2u64.saturating_pow(step.min(4)))
        .min(RATE_LIMIT_BACKOFF_MAX_MS);
    Duration::from_millis(ms)
}

/// Maximum concurrent `getTransaction` fetches across the whole process.
/// History reconstruction, live polling, and the exit monitor share one
/// RpcPool; this prevents an uncontrolled burst of transaction requests
/// against rate-limited public endpoints.
const MAX_CONCURRENT_TX_FETCHES: usize = 2;

#[derive(Debug, Error)]
pub enum RpcError {
    #[error("no RPC endpoint succeeded: {0}")]
    Unavailable(String),
    #[error("HTTP client: {0}")]
    Http(#[from] reqwest::Error),
    #[error("invalid RPC response: {0}")]
    Invalid(String),
}
impl RpcError {
    /// An availability failure never proves anything about a submitted
    /// transaction; callers must treat it as "state unknown".
    pub fn is_availability(&self) -> bool {
        matches!(self, Self::Unavailable(_) | Self::Http(_))
    }
}
#[derive(Clone)]
pub struct RpcPool {
    client: Client,
    endpoints: Vec<String>,
    #[allow(dead_code)]
    timeout: Duration,
    max_attempts: u32,
    /// Global concurrency gate for `getTransaction` fetches, shared across
    /// every clone of the pool so all callers observe one bounded window.
    tx_gate: Arc<tokio::sync::Semaphore>,
}
#[derive(Debug, Clone)]
pub struct RpcObservation {
    pub value: Value,
    pub observed_at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
}

/// Confirmed on-chain status of a signature, as reported by an RPC node.
#[derive(Debug, Clone)]
pub struct SignatureStatus {
    pub err: Option<Value>,
    pub confirmation_status: Option<String>,
    pub slot: u64,
}
impl SignatureStatus {
    pub fn is_success(&self) -> bool {
        self.err.is_none()
    }
    pub fn is_confirmed_or_finalized(&self) -> bool {
        matches!(
            self.confirmation_status.as_deref(),
            Some("confirmed") | Some("finalized")
        )
    }
}
/// One SPL token account owned by the wallet, parsed from jsonParsed RPC data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenBalance {
    pub mint: String,
    pub amount: u64,
    pub decimals: u8,
}

pub const SOL_TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

/// A single transaction signature entry from `getSignaturesForAddress`.
#[derive(Debug, Clone)]
pub struct SignatureEntry {
    pub signature: String,
    pub slot: u64,
    pub block_time: Option<i64>,
    pub err: Option<Value>,
    pub confirmation_status: Option<String>,
}

/// One of the largest token holders, from `getTokenLargestAccounts`.
#[derive(Debug, Clone)]
pub struct TokenLargestAccount {
    pub address: String,
    pub amount: u64,
    pub decimals: u8,
}

/// Parsed SPL Token Mint account info from `getAccountInfo` (jsonParsed).
#[derive(Debug, Clone)]
pub struct MintAccountInfo {
    pub mint_authority: Option<String>,
    pub freeze_authority: Option<String>,
    pub supply: u64,
    pub decimals: u8,
    pub is_initialized: bool,
}

/// Health check result for a single RPC endpoint.
#[derive(Debug, Clone)]
pub struct RpcHealthEntry {
    pub endpoint: String,
    pub healthy: bool,
    pub latency_ms: u64,
    pub status_code: u16,
    #[allow(dead_code)]
    pub error: Option<String>,
}

impl RpcPool {
    pub fn new(endpoints: Vec<String>, timeout: Duration) -> Result<Self, RpcError> {
        Self::with_attempts(endpoints, timeout, 1)
    }
    pub fn with_attempts(
        endpoints: Vec<String>,
        #[allow(dead_code)] timeout: Duration,
        max_attempts: u32,
    ) -> Result<Self, RpcError> {
        let client = Client::builder().timeout(timeout).build()?;
        Ok(Self {
            client,
            endpoints,
            max_attempts: max_attempts.max(1),
            timeout,
            tx_gate: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_TX_FETCHES)),
        })
    }
    pub fn endpoints(&self) -> &[String] {
        &self.endpoints
    }
    /// Tries every endpoint up to `max_attempts` passes with bounded backoff.
    /// A timeout or connection error on one endpoint is an availability
    /// problem, never evidence about transaction state.
    ///
    /// HTTP 429 responses get dedicated handling: the bounded exponential
    /// backoff ladder (350ms → 700ms → 1.4s → 2.8s → 5s max) applies, a
    /// `Retry-After` header is honored (capped so a single response cannot
    /// stall the loop), and the pool fails over to the next endpoint after
    /// repeated 429s. Total tries are bounded by `max_attempts` passes over
    /// all endpoints — never an infinite retry.
    pub async fn call(&self, method: &str, params: Value) -> Result<RpcObservation, RpcError> {
        let observed_at = Utc::now();
        let mut errors = Vec::new();
        let mut rate_limit_step: u32 = 0;
        for attempt in 0..self.max_attempts {
            if attempt > 0 {
                // Non-429 failures: bounded generic backoff before the next
                // pass. 429s already consumed their own backoff inline.
                let backoff_ms = 500u64.saturating_mul(2u64.saturating_pow(attempt.min(4)));
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            }
            for (endpoint_idx, endpoint) in self.endpoints.iter().enumerate() {
                let body = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params});
                match self.client.post(endpoint).json(&body).send().await {
                    Ok(r) => {
                        let status = r.status();
                        if status.as_u16() == 429 {
                            let retry_after = r
                                .headers()
                                .get("retry-after")
                                .and_then(|v| v.to_str().ok())
                                .and_then(|s| s.parse::<u64>().ok());
                            let backoff = match retry_after {
                                // Retry-After is honored, capped at the same
                                // 5s ladder maximum so the loop stays bounded.
                                Some(secs) => Duration::from_secs(secs)
                                    .min(Duration::from_millis(RATE_LIMIT_BACKOFF_MAX_MS)),
                                None => rate_limit_backoff(rate_limit_step),
                            };
                            rate_limit_step = rate_limit_step.saturating_add(1);
                            let failover = if endpoint_idx + 1 < self.endpoints.len() {
                                "failing over to next endpoint"
                            } else {
                                "retrying after backoff"
                            };
                            tracing::debug!(
                                method = %method,
                                endpoint = %endpoint,
                                backoff_ms = backoff.as_millis() as u64,
                                ?retry_after,
                                rate_limit_step,
                                "RPC 429; backing off then {}",
                                failover
                            );
                            tokio::time::sleep(backoff).await;
                            errors.push(format!("{endpoint} [{status}]: 429 rate-limited"));
                            continue; // fail over to the next endpoint
                        }
                        match r.json::<Value>().await {
                            Ok(v) if v.get("error").is_none() => {
                                return Ok(RpcObservation {
                                    value: v["result"].clone(),
                                    observed_at,
                                    received_at: Utc::now(),
                                })
                            }
                            Ok(v) => {
                                let err_msg = v
                                    .get("error")
                                    .and_then(|e| e.get("message"))
                                    .and_then(|m| m.as_str())
                                    .unwrap_or("unknown");
                                errors.push(format!("{endpoint} [{status}]: {err_msg}"));
                            }
                            Err(e) => errors.push(format!("{endpoint} [{status}]: {e}")),
                        }
                    }
                    Err(e) => errors.push(format!("{endpoint}: {e}")),
                }
            }
        }
        Err(RpcError::Unavailable(errors.join("; ")))
    }
    pub async fn health(&self) -> Result<(), RpcError> {
        self.call("getHealth", json!([])).await.map(|_| ())
    }

    /// Performs a health check across all configured RPC endpoints and returns
    /// a per-endpoint report. If any endpoint disagrees on a critical value
    /// (e.g., slot health), the contradictory provider is flagged.
    pub async fn health_report(&self) -> Vec<RpcHealthEntry> {
        let mut report = Vec::with_capacity(self.endpoints.len());
        for endpoint in &self.endpoints {
            let start = std::time::Instant::now();
            let result = self
                .client
                .post(endpoint)
                .json(&json!({"jsonrpc":"2.0","id":1,"method":"getHealth","params":[]}))
                .send()
                .await;
            let latency_ms = start.elapsed().as_millis() as u64;
            match result {
                Ok(r) => {
                    let status = r.status();
                    let is_ok = status.is_success();
                    report.push(RpcHealthEntry {
                        endpoint: endpoint.clone(),
                        healthy: is_ok,
                        latency_ms,
                        status_code: status.as_u16(),
                        error: None,
                    });
                }
                Err(e) => {
                    report.push(RpcHealthEntry {
                        endpoint: endpoint.clone(),
                        healthy: false,
                        latency_ms,
                        status_code: 0,
                        error: Some(e.to_string()),
                    });
                }
            }
        }
        report
    }
    pub async fn balance_lamports(&self, address: &str) -> Result<u64, RpcError> {
        let v = self
            .call("getBalance", json!([address,{"commitment":"confirmed"}]))
            .await?;
        v.value["value"]
            .as_u64()
            .ok_or_else(|| RpcError::Invalid("missing balance value".into()))
    }
    /// `None` means the node has not seen the signature (which is not proof
    /// of failure); `Some` carries the on-chain verdict.
    pub async fn signature_status(
        &self,
        signature: &str,
    ) -> Result<Option<SignatureStatus>, RpcError> {
        let v = self
            .call(
                "getSignatureStatuses",
                json!([[signature],{"searchTransactionHistory":true}]),
            )
            .await?;
        let entry = &v.value["value"][0];
        if entry.is_null() {
            return Ok(None);
        }
        let err = if entry["err"].is_null() {
            None
        } else {
            Some(entry["err"].clone())
        };
        Ok(Some(SignatureStatus {
            err,
            confirmation_status: entry["confirmationStatus"].as_str().map(str::to_owned),
            slot: entry["slot"].as_u64().unwrap_or(0),
        }))
    }
    /// Full transaction with metadata, required to verify the actual swap
    /// outcome (pre/post token balances and fees). `None` = not indexed yet.
    ///
    /// Gated by the pool-wide semaphore: at most `MAX_CONCURRENT_TX_FETCHES`
    /// transaction fetches run at once across all tasks (history
    /// reconstruction, live polling, exit monitor), preventing uncontrolled
    /// request bursts against rate-limited endpoints.
    pub async fn transaction(&self, signature: &str) -> Result<Option<Value>, RpcError> {
        let _permit = self
            .tx_gate
            .acquire()
            .await
            .map_err(|_| RpcError::Unavailable("transaction gate closed".into()))?;
        let v=self.call("getTransaction",json!([signature,{"encoding":"json","commitment":"confirmed","maxSupportedTransactionVersion":0}])).await?;
        Ok(if v.value.is_null() {
            None
        } else {
            Some(v.value)
        })
    }
    /// All SPL token accounts owned by `owner`. Used for restart
    /// reconciliation; a failure here must block trading, not be guessed away.
    /// Fetch recent transaction signatures for an address.
    pub async fn signatures_for_address(
        &self,
        address: &str,
        limit: u32,
    ) -> Result<Vec<SignatureEntry>, RpcError> {
        tracing::info!(
            address = %address,
            limit = limit,
            endpoints = ?self.endpoints.iter().map(|e| &e[..50.min(e.len())]).collect::<Vec<_>>(),
            "calling getSignaturesForAddress"
        );
        let v = self
            .call(
                "getSignaturesForAddress",
                json!([address, {"limit": limit, "commitment": "confirmed"}]),
            )
            .await?;
        let entries = v
            .value
            .as_array()
            .ok_or_else(|| RpcError::Invalid("missing signatures array".into()))?;
        tracing::info!(
            address = %address,
            returned = entries.len(),
            "getSignaturesForAddress response received"
        );
        let mut out = Vec::with_capacity(entries.len());
        for e in entries {
            let signature = e["signature"]
                .as_str()
                .ok_or_else(|| RpcError::Invalid("signature entry missing signature".into()))?
                .to_string();
            let slot = e["slot"].as_u64().unwrap_or(0);
            let block_time = e["blockTime"].as_i64();
            let err = if e["err"].is_null() {
                None
            } else {
                Some(e["err"].clone())
            };
            let confirmation_status = e["confirmationStatus"].as_str().map(str::to_owned);
            out.push(SignatureEntry {
                signature,
                slot,
                block_time,
                err,
                confirmation_status,
            });
        }
        Ok(out)
    }

    /// Paginated variant of `signatures_for_address` that walks backward in
    /// time using the `before` cursor.  Returns a partial page (and no error)
    /// when the chain is exhausted.
    pub async fn signatures_for_address_paged(
        &self,
        address: &str,
        limit: u32,
        before: Option<&str>,
    ) -> Result<Vec<SignatureEntry>, RpcError> {
        let mut options = json!({"limit": limit, "commitment": "confirmed"});
        if let Some(b) = before {
            options["before"] = json!(b);
        }
        let v = self
            .call("getSignaturesForAddress", json!([address, options]))
            .await?;
        let entries = v
            .value
            .as_array()
            .ok_or_else(|| RpcError::Invalid("missing signatures array".into()))?;
        let mut out = Vec::with_capacity(entries.len());
        for e in entries {
            let signature = e["signature"]
                .as_str()
                .ok_or_else(|| RpcError::Invalid("signature entry missing signature".into()))?
                .to_string();
            let slot = e["slot"].as_u64().unwrap_or(0);
            let block_time = e["blockTime"].as_i64();
            let err = if e["err"].is_null() {
                None
            } else {
                Some(e["err"].clone())
            };
            let confirmation_status = e["confirmationStatus"].as_str().map(str::to_owned);
            out.push(SignatureEntry {
                signature,
                slot,
                block_time,
                err,
                confirmation_status,
            });
        }
        Ok(out)
    }

    /// Fetch the largest token accounts for a mint (holder concentration analysis).
    pub async fn token_largest_accounts(
        &self,
        mint: &str,
    ) -> Result<Vec<TokenLargestAccount>, RpcError> {
        let v = self
            .call(
                "getTokenLargestAccounts",
                json!([mint, {"commitment": "confirmed"}]),
            )
            .await?;
        let value = &v.value["value"];
        let accounts = value
            .as_array()
            .ok_or_else(|| RpcError::Invalid("missing token largest accounts array".into()))?;
        let mut out = Vec::with_capacity(accounts.len());
        for a in accounts {
            let address = a["address"]
                .as_str()
                .ok_or_else(|| RpcError::Invalid("largest account missing address".into()))?
                .to_string();
            let amount_str = a["amount"]
                .as_str()
                .ok_or_else(|| RpcError::Invalid("largest account missing amount".into()))?;
            let amount = amount_str
                .parse::<u64>()
                .map_err(|_| RpcError::Invalid("invalid largest account amount".into()))?;
            let decimals = a["decimals"].as_u64().unwrap_or(0) as u8;
            out.push(TokenLargestAccount {
                address,
                amount,
                decimals,
            });
        }
        Ok(out)
    }

    /// Fetch parsed SPL Token Mint account info (mint_authority, freeze_authority, supply).
    /// Returns `None` if the account does not exist.
    pub async fn mint_account_info(&self, mint: &str) -> Result<Option<MintAccountInfo>, RpcError> {
        let v = self
            .call(
                "getAccountInfo",
                json!([mint, {"encoding": "jsonParsed", "commitment": "confirmed"}]),
            )
            .await?;
        let account = &v.value["value"];
        if account.is_null() {
            return Ok(None);
        }
        let data = &account["data"];
        let parsed = &data["parsed"];
        let info = &parsed["info"];
        let mint_authority = info["mintAuthority"].as_str().map(str::to_owned);
        let freeze_authority = info["freezeAuthority"].as_str().map(str::to_owned);
        let supply_str = info["supply"]
            .as_str()
            .ok_or_else(|| RpcError::Invalid("mint missing supply".into()))?;
        let supply = supply_str
            .parse::<u64>()
            .map_err(|_| RpcError::Invalid("invalid mint supply".into()))?;
        let decimals = info["decimals"]
            .as_u64()
            .ok_or_else(|| RpcError::Invalid("mint missing decimals".into()))?
            as u8;
        let is_initialized = info["isInitialized"].as_bool().unwrap_or(false);
        Ok(Some(MintAccountInfo {
            mint_authority,
            freeze_authority,
            supply,
            decimals,
            is_initialized,
        }))
    }

    pub async fn token_balances(&self, owner: &str) -> Result<Vec<TokenBalance>, RpcError> {
        let v=self.call("getTokenAccountsByOwner",json!([owner,{"programId":SOL_TOKEN_PROGRAM},{"encoding":"jsonParsed","commitment":"confirmed"}])).await?;
        let accounts = v.value["value"]
            .as_array()
            .ok_or_else(|| RpcError::Invalid("missing token account array".into()))?;
        let mut out = Vec::new();
        for a in accounts {
            let info = &a["account"]["data"]["parsed"]["info"];
            let mint = info["mint"]
                .as_str()
                .ok_or_else(|| RpcError::Invalid("token account missing mint".into()))?;
            let amount_str = info["tokenAmount"]["amount"]
                .as_str()
                .ok_or_else(|| RpcError::Invalid("token account missing amount".into()))?;
            let decimals = info["tokenAmount"]["decimals"]
                .as_u64()
                .ok_or_else(|| RpcError::Invalid("token account missing decimals".into()))?;
            out.push(TokenBalance {
                mint: mint.to_owned(),
                amount: amount_str
                    .parse::<u64>()
                    .map_err(|_| RpcError::Invalid("invalid token amount".into()))?,
                decimals: u8::try_from(decimals)
                    .map_err(|_| RpcError::Invalid("invalid token decimals".into()))?,
            });
        }
        Ok(out)
    }

    /// Fetch multiple accounts in a single RPC call using base64 encoding.
    /// Returns a vec of `Option<String>` where each element is the base64-encoded
    /// account data, or `None` if the account does not exist.
    pub async fn fetch_accounts_base64(
        &self,
        pubkeys: &[&str],
    ) -> Result<Vec<Option<String>>, RpcError> {
        if pubkeys.is_empty() {
            return Ok(vec![]);
        }
        let v = self
            .call(
                "getMultipleAccounts",
                json!([pubkeys, {"encoding": "base64", "commitment": "confirmed"}]),
            )
            .await?;
        let accounts = v.value["value"].as_array().ok_or_else(|| {
            RpcError::Invalid("missing value array in getMultipleAccounts".into())
        })?;
        let mut result = Vec::with_capacity(pubkeys.len());
        for account in accounts {
            if account.is_null() {
                result.push(None);
            } else {
                let data = account["data"]
                    .as_array()
                    .and_then(|arr| arr.get(1))
                    .and_then(|d| d.as_str())
                    .map(str::to_owned);
                result.push(data);
            }
        }
        Ok(result)
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn availability_errors_are_not_failure_evidence() {
        assert!(RpcError::Unavailable("x".into()).is_availability());
        assert!(!RpcError::Invalid("x".into()).is_availability());
    }

    // --- RPC 429 backoff regressions ---

    #[test]
    fn rate_limit_backoff_follows_bounded_ladder() {
        // 350ms → 700ms → 1.4s → 2.8s → 5s max, never beyond.
        assert_eq!(rate_limit_backoff(0), Duration::from_millis(350));
        assert_eq!(rate_limit_backoff(1), Duration::from_millis(700));
        assert_eq!(rate_limit_backoff(2), Duration::from_millis(1400));
        assert_eq!(rate_limit_backoff(3), Duration::from_millis(2800));
        assert_eq!(rate_limit_backoff(4), Duration::from_millis(5000));
        assert_eq!(rate_limit_backoff(5), Duration::from_millis(5000));
        assert_eq!(rate_limit_backoff(50), Duration::from_millis(5000));
    }

    #[test]
    fn retry_after_is_respected_but_capped() {
        let cap = Duration::from_millis(RATE_LIMIT_BACKOFF_MAX_MS);
        // A Retry-After within the cap is honored verbatim.
        let secs = 3u64;
        assert_eq!(Duration::from_secs(secs).min(cap), Duration::from_secs(3));
        // A Retry-After beyond the cap is clamped so the loop stays bounded.
        assert_eq!(
            Duration::from_secs(600).min(cap),
            Duration::from_millis(5000)
        );
    }

    #[test]
    fn tx_gate_limits_concurrency_to_configured_permits() {
        let pool = RpcPool::with_attempts(
            vec!["http://localhost:1".into()],
            Duration::from_millis(50),
            1,
        )
        .unwrap();
        // Two permits total: acquiring both leaves the gate closed for a
        // third fetch until one is released.
        let p1 = pool.tx_gate.clone().try_acquire_owned().unwrap();
        let p2 = pool.tx_gate.clone().try_acquire_owned().unwrap();
        assert!(pool.tx_gate.try_acquire().is_err());
        drop(p1);
        drop(p2);
        assert!(pool.tx_gate.try_acquire().is_ok());
    }

    #[test]
    fn clone_preserves_configured_endpoints() {
        let rpc = RpcPool::with_attempts(
            vec![
                "https://primary.example".into(),
                "https://backup.example".into(),
            ],
            Duration::from_secs(1),
            2,
        )
        .unwrap();
        let cloned = rpc.clone();
        assert_eq!(rpc.endpoints(), cloned.endpoints());
        assert_eq!(
            rpc.endpoints(),
            &[
                "https://primary.example".to_string(),
                "https://backup.example".to_string()
            ]
        );
        assert!(!rpc.endpoints().iter().any(|e| e.contains("127.0.0.1:1")));
    }
    #[test]
    fn status_classification() {
        let s = SignatureStatus {
            err: None,
            confirmation_status: Some("finalized".into()),
            slot: 1,
        };
        assert!(s.is_success() && s.is_confirmed_or_finalized());
        let s = SignatureStatus {
            err: Some(json!("AccountInUse")),
            confirmation_status: Some("confirmed".into()),
            slot: 1,
        };
        assert!(!s.is_success() && s.is_confirmed_or_finalized());
        let s = SignatureStatus {
            err: None,
            confirmation_status: Some("processed".into()),
            slot: 1,
        };
        assert!(!s.is_confirmed_or_finalized());
    }
}
