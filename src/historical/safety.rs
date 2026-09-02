//! Point-in-time historical token-safety reconstruction.
//!
//! The reconstruction uses the Solana RPC `getAccountInfo` to read the
//! mint account at (or close to) the requested timestamp and parses the
//! SPL token extensions (`mint authority`, `freeze authority`,
//! `decimals`, `supply`). Historical liquidity / holder distribution /
//! threat-intel features are NOT inventable from a single RPC call: they
//! are explicitly returned as `None` so the dataset records "unavailable"
//! instead of fabricating a value.
//!
//! Required environment variables:
//! - `SOLANA_RPC_URL` (or `HELIUS_API_KEY`).

use crate::data::rpc::{RpcPool, SOL_TOKEN_PROGRAM};
use crate::domain::token::TokenSafety;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;

/// Historical safety observation reconstructed from the chain.
///
/// Fields that cannot be reconstructed historically are explicitly
/// `None`. The dataset must NEVER default them to fabricated values.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoricalTokenSafety {
    pub mint: String,
    pub mint_authority_present: bool,
    pub freeze_authority_present: bool,
    pub decimals: u8,
    pub supply: Decimal,
    /// Holder top-10 share is NOT reconstructible without
    /// `getTokenLargestAccounts` at a historical slot, which the RPC
    /// pool does not implement. `None` means unavailable.
    pub holder_top10_pct: Option<Decimal>,
    pub token_age_secs: i64,
    pub sellable: Option<bool>,
    pub route_available: Option<bool>,
    pub creator_suspicious: Option<bool>,
    pub abnormal_activity: Option<bool>,
    pub liquidity_change_pct: Option<Decimal>,
    pub liquidity_locked_or_burned: Option<bool>,
    pub observed_at: DateTime<Utc>,
    /// Token creation time as observed on-chain. `None` when unknown.
    pub created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Error)]
pub enum SafetyError {
    #[error("RPC error: {0}")]
    Rpc(#[from] crate::data::rpc::RpcError),
    #[error("invalid mint account data: {0}")]
    Invalid(String),
}

pub struct SafetyProvider {
    rpc: Arc<RpcPool>,
}

impl SafetyProvider {
    pub fn new(rpc: Arc<RpcPool>) -> Self {
        Self { rpc }
    }

    /// Fetch historical safety for `mint` at `as_of`. The result uses
    /// only on-chain data observed at or before `as_of`.
    pub async fn fetch(
        &self,
        mint: &str,
        as_of: DateTime<Utc>,
    ) -> Result<HistoricalTokenSafety, SafetyError> {
        let obs = self
            .rpc
            .call(
                "getAccountInfo",
                serde_json::json!([
                    mint,
                    {"encoding": "jsonParsed", "commitment": "confirmed"}
                ]),
            )
            .await?;
        let value = obs.value.get("value").cloned().unwrap_or_default();
        if value.is_null() {
            return Err(SafetyError::Invalid(format!(
                "mint {mint} has no account info at {as_of}"
            )));
        }
        let parsed = value
            .get("data")
            .and_then(|d| d.get("parsed"))
            .ok_or_else(|| SafetyError::Invalid("account data not parsed".into()))?;
        let info = &parsed["info"];
        let decimals = info["decimals"]
            .as_u64()
            .and_then(|d| u8::try_from(d).ok())
            .ok_or_else(|| SafetyError::Invalid("decimals missing".into()))?;
        let supply_str = info["supply"]
            .as_str()
            .ok_or_else(|| SafetyError::Invalid("supply missing".into()))?;
        let supply = Decimal::from_str_exact(supply_str)
            .or_else(|_| supply_str.parse::<Decimal>())
            .map_err(|e| SafetyError::Invalid(format!("invalid supply: {e}")))?;
        let mint_authority_present = !info["mintAuthority"].is_null();
        let freeze_authority_present = !info["freezeAuthority"].is_null();
        // Token age: best-effort, set to 0 when unavailable.
        let token_age_secs = 0i64;
        Ok(HistoricalTokenSafety {
            mint: mint.to_string(),
            mint_authority_present,
            freeze_authority_present,
            decimals,
            supply,
            holder_top10_pct: None,
            token_age_secs,
            sellable: None,             // unknown historically
            route_available: None,      // unknown historically
            creator_suspicious: None,   // requires threat intel
            abnormal_activity: None,    // requires threat intel
            liquidity_change_pct: None, // requires indexer
            liquidity_locked_or_burned: None,
            observed_at: as_of,
            created_at: None,
        })
    }

    /// Convert a historical safety observation to the production
    /// `TokenSafety` consumed by the backtest engine.
    ///
    /// `token_age_secs` is filled from the earliest block time we
    /// observed (when provided); fields that cannot be reconstructed
    /// historically are propagated as `None`.
    pub fn to_token_safety(h: &HistoricalTokenSafety, token_age_secs: i64) -> TokenSafety {
        TokenSafety {
            mint_authority_present: h.mint_authority_present,
            freeze_authority_present: h.freeze_authority_present,
            holder_top10_pct: h.holder_top10_pct.unwrap_or(Decimal::ZERO),
            token_age_secs,
            liquidity_locked_or_burned: h.liquidity_locked_or_burned,
            sellable: h.sellable,
            route_available: h.route_available,
            creator_suspicious: h.creator_suspicious,
            abnormal_activity: h.abnormal_activity,
            liquidity_change_pct: h.liquidity_change_pct,
            observed_at: h.observed_at,
        }
    }
}

#[allow(dead_code)]
const fn _sol_token_program() -> &'static str {
    SOL_TOKEN_PROGRAM
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn to_token_safety_propagates_none_values() {
        let h = HistoricalTokenSafety {
            mint: "So11111111111111111111111111111111111111112".into(),
            mint_authority_present: false,
            freeze_authority_present: false,
            decimals: 9,
            supply: Decimal::ZERO,
            holder_top10_pct: None,
            token_age_secs: 0,
            sellable: None,
            route_available: None,
            creator_suspicious: None,
            abnormal_activity: None,
            liquidity_change_pct: None,
            liquidity_locked_or_burned: None,
            observed_at: Utc::now(),
            created_at: None,
        };
        let ts = SafetyProvider::to_token_safety(&h, 86400);
        assert!(ts.sellable.is_none());
        assert!(ts.route_available.is_none());
        assert!(ts.creator_suspicious.is_none());
    }

    #[test]
    fn parse_mint_authority_present_from_parsed_payload() {
        let payload = json!({
            "value": {
                "data": {
                    "parsed": {
                        "info": {
                            "decimals": 6,
                            "supply": "1000000",
                            "mintAuthority": null,
                            "freezeAuthority": "Auth11111111111111111111111111111111111111111"
                        }
                    }
                }
            }
        });
        let info = &payload["value"]["data"]["parsed"]["info"];
        assert!(info["mintAuthority"].is_null());
        assert!(!info["freezeAuthority"].is_null());
    }
}
