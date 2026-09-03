use chrono::{DateTime, Utc};
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;
use thiserror::Error;

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
        })
    }
    pub fn endpoints(&self) -> &[String] {
        &self.endpoints
    }
    /// Tries every endpoint up to `max_attempts` passes with bounded backoff.
    /// A timeout or connection error on one endpoint is an availability
    /// problem, never evidence about transaction state.
    pub async fn call(&self, method: &str, params: Value) -> Result<RpcObservation, RpcError> {
        let observed_at = Utc::now();
        let mut errors = Vec::new();
        for attempt in 0..self.max_attempts {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(
                    250u64.saturating_mul(2u64.saturating_pow(attempt.min(4))),
                ))
                .await;
            }
            for endpoint in &self.endpoints {
                let body = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params});
                match self.client.post(endpoint).json(&body).send().await {
                    Ok(r) => match r.json::<Value>().await {
                        Ok(v) if v.get("error").is_none() => {
                            return Ok(RpcObservation {
                                value: v["result"].clone(),
                                observed_at,
                                received_at: Utc::now(),
                            })
                        }
                        Ok(v) => errors.push(format!("{endpoint}: {}", v["error"])),
                        Err(e) => errors.push(format!("{endpoint}: {e}")),
                    },
                    Err(e) => errors.push(format!("{endpoint}: {e}")),
                }
            }
        }
        Err(RpcError::Unavailable(errors.join("; ")))
    }
    pub async fn health(&self) -> Result<(), RpcError> {
        self.call("getHealth", json!([])).await.map(|_| ())
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
    pub async fn transaction(&self, signature: &str) -> Result<Option<Value>, RpcError> {
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
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn availability_errors_are_not_failure_evidence() {
        assert!(RpcError::Unavailable("x".into()).is_availability());
        assert!(!RpcError::Invalid("x".into()).is_availability());
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
