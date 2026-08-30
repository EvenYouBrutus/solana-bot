use chrono::{DateTime, Utc}; use reqwest::Client; use serde_json::{json, Value}; use std::time::Duration; use thiserror::Error;

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
    pub fn is_availability(&self) -> bool { matches!(self, Self::Unavailable(_) | Self::Http(_)) }
}
#[derive(Clone)] pub struct RpcPool { client: Client, endpoints: Vec<String>, timeout: Duration, max_attempts: u32 }
#[derive(Debug, Clone)] pub struct RpcObservation { pub value: Value, pub observed_at: DateTime<Utc>, pub received_at: DateTime<Utc> }

/// Confirmed on-chain status of a signature, as reported by an RPC node.
#[derive(Debug, Clone)]
pub struct SignatureStatus { pub err: Option<Value>, pub confirmation_status: Option<String>, pub slot: u64 }
impl SignatureStatus {
    pub fn is_success(&self) -> bool { self.err.is_none() }
    pub fn is_confirmed_or_finalized(&self) -> bool { matches!(self.confirmation_status.as_deref(), Some("confirmed") | Some("finalized")) }
}
/// One SPL token account owned by the wallet, parsed from jsonParsed RPC data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenBalance { pub mint: String, pub amount: u64, pub decimals: u8 }

pub const SOL_TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

impl RpcPool {
    pub fn new(endpoints: Vec<String>, timeout: Duration) -> Result<Self, RpcError> { Self::with_attempts(endpoints, timeout, 1) }
    pub fn with_attempts(endpoints: Vec<String>, timeout: Duration, max_attempts: u32) -> Result<Self, RpcError> {
        let client=Client::builder().timeout(timeout).build()?; Ok(Self{client,endpoints,max_attempts: max_attempts.max(1),timeout})
    }
    /// Tries every endpoint up to `max_attempts` passes with bounded backoff.
    /// A timeout or connection error on one endpoint is an availability
    /// problem, never evidence about transaction state.
    pub async fn call(&self, method: &str, params: Value) -> Result<RpcObservation,RpcError> {
        let observed_at=Utc::now();
        let mut errors=Vec::new();
        for attempt in 0..self.max_attempts {
            if attempt > 0 { tokio::time::sleep(Duration::from_millis(250u64.saturating_mul(2u64.saturating_pow(attempt.min(4))))).await; }
            for endpoint in &self.endpoints {
                let body=json!({"jsonrpc":"2.0","id":1,"method":method,"params":params});
                match self.client.post(endpoint).json(&body).send().await {
                    Ok(r) => match r.json::<Value>().await {
                        Ok(v) if v.get("error").is_none() => return Ok(RpcObservation{value:v["result"].clone(),observed_at,received_at:Utc::now()}),
                        Ok(v)=>errors.push(format!("{endpoint}: {}",v["error"])),
                        Err(e)=>errors.push(format!("{endpoint}: {e}")),
                    },
                    Err(e)=>errors.push(format!("{endpoint}: {e}")),
                }
            }
        }
        Err(RpcError::Unavailable(errors.join("; ")))
    }
    pub async fn health(&self)->Result<(),RpcError>{self.call("getHealth",json!([])).await.map(|_|())}
    pub async fn balance_lamports(&self,address:&str)->Result<u64,RpcError>{let v=self.call("getBalance",json!([address,{"commitment":"confirmed"}])).await?;v.value["value"].as_u64().ok_or_else(||RpcError::Invalid("missing balance value".into()))}
    /// `None` means the node has not seen the signature (which is not proof
    /// of failure); `Some` carries the on-chain verdict.
    pub async fn signature_status(&self, signature:&str)->Result<Option<SignatureStatus>,RpcError>{
        let v=self.call("getSignatureStatuses",json!([[signature],{"searchTransactionHistory":true}])).await?;
        let entry=&v.value["value"][0];
        if entry.is_null(){return Ok(None)}
        let err=if entry["err"].is_null(){None}else{Some(entry["err"].clone())};
        Ok(Some(SignatureStatus{err,confirmation_status:entry["confirmationStatus"].as_str().map(str::to_owned),slot:entry["slot"].as_u64().unwrap_or(0)}))
    }
    /// Full transaction with metadata, required to verify the actual swap
    /// outcome (pre/post token balances and fees). `None` = not indexed yet.
    pub async fn transaction(&self, signature:&str)->Result<Option<Value>,RpcError>{
        let v=self.call("getTransaction",json!([signature,{"encoding":"json","commitment":"confirmed","maxSupportedTransactionVersion":0}])).await?;
        Ok(if v.value.is_null(){None}else{Some(v.value)})
    }
    /// All SPL token accounts owned by `owner`. Used for restart
    /// reconciliation; a failure here must block trading, not be guessed away.
    pub async fn token_balances(&self, owner:&str)->Result<Vec<TokenBalance>,RpcError>{
        let v=self.call("getTokenAccountsByOwner",json!([owner,{"programId":SOL_TOKEN_PROGRAM},{"encoding":"jsonParsed","commitment":"confirmed"}])).await?;
        let accounts=v.value["value"].as_array().ok_or_else(||RpcError::Invalid("missing token account array".into()))?;
        let mut out=Vec::new();
        for a in accounts {
            let info=&a["account"]["data"]["parsed"]["info"];
            let mint=info["mint"].as_str().ok_or_else(||RpcError::Invalid("token account missing mint".into()))?;
            let amount_str=info["tokenAmount"]["amount"].as_str().ok_or_else(||RpcError::Invalid("token account missing amount".into()))?;
            let decimals=info["tokenAmount"]["decimals"].as_u64().ok_or_else(||RpcError::Invalid("token account missing decimals".into()))?;
            out.push(TokenBalance{mint:mint.to_owned(),amount:amount_str.parse::<u64>().map_err(|_|RpcError::Invalid("invalid token amount".into()))?,decimals:u8::try_from(decimals).map_err(|_|RpcError::Invalid("invalid token decimals".into()))?});
        }
        Ok(out)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn availability_errors_are_not_failure_evidence() { assert!(RpcError::Unavailable("x".into()).is_availability()); assert!(RpcError::Invalid("x".into()).is_availability() == false); }
    #[test] fn status_classification() {
        let s=SignatureStatus{err:None,confirmation_status:Some("finalized".into()),slot:1}; assert!(s.is_success() && s.is_confirmed_or_finalized());
        let s=SignatureStatus{err:Some(json!("AccountInUse")),confirmation_status:Some("confirmed".into()),slot:1}; assert!(!s.is_success() && s.is_confirmed_or_finalized());
        let s=SignatureStatus{err:None,confirmation_status:Some("processed".into()),slot:1}; assert!(!s.is_confirmed_or_finalized());
    }
}
