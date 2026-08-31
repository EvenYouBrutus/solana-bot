use crate::economics::CostModel;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Lifecycle of a position. `Open` positions are eligible for exits; `Closed`
/// positions are retained purely as auditable history.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum PositionState {
    #[default]
    Open,
    Closed,
}
/// Outcome of comparing internal accounting with on-chain token balances.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum ReconciliationStatus {
    #[default]
    Unverified,
    Reconciled,
    AdjustedOnChain,
    Mismatch,
    ClosedOnChain,
}
impl ReconciliationStatus {
    /// A position may only be sold when its quantity is trusted.
    pub fn quantity_is_trusted(self) -> bool {
        matches!(self, Self::Reconciled | Self::AdjustedOnChain)
    }
}

/// Accounting fields added after the initial schema. `None` means a legacy
/// record did not contain the evidence; callers must not infer it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub mint: String,
    #[serde(default)]
    pub position_id: Option<String>,
    #[serde(default)]
    pub token_mint: Option<String>,
    #[serde(default)]
    pub base_mint: Option<String>,
    #[serde(default)]
    pub entry_input_amount_atomic: Option<u64>,
    #[serde(default)]
    pub entry_output_amount_atomic: Option<u64>,
    #[serde(default)]
    pub token_decimals: Option<u8>,
    #[serde(default)]
    pub base_mint_decimals: Option<u8>,
    #[serde(default)]
    pub entry_fees_usd: Option<Decimal>,
    #[serde(default)]
    pub entry_slippage_bps: Option<u32>,
    #[serde(default)]
    pub entry_cost_model: Option<CostModel>,
    /// Atomic (raw) token quantity credited at entry; integer accounting.
    pub quantity: Decimal,
    /// Remaining unsold quantity in atomic units. `None` on legacy records;
    /// exits are refused until reconciliation establishes the true balance.
    #[serde(default)]
    pub remaining_quantity_atomic: Option<u64>,
    /// USD actually paid at entry (verified fill basis, including nothing else).
    #[serde(default)]
    pub entry_cost_usd: Option<Decimal>,
    /// USD value of one base-mint unit implied by the entry fill; the mark
    /// basis for exit proceeds when no fresher base price is available.
    #[serde(default)]
    pub base_entry_price_usd: Option<Decimal>,
    #[serde(default)]
    pub state: PositionState,
    #[serde(default)]
    pub reconciliation_status: ReconciliationStatus,
    #[serde(default)]
    pub last_reconciled_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub exit_signature: Option<String>,
    #[serde(default)]
    pub exit_fees_usd: Option<Decimal>,
    #[serde(default)]
    pub exit_time: Option<DateTime<Utc>>,
    pub entry_price_usd: Decimal,
    pub entry_time: DateTime<Utc>,
    pub entry_signature: String,
    pub high_water_price_usd: Decimal,
    pub realized_pnl_usd: Decimal,
    pub unrealized_pnl_usd: Decimal,
    pub fees_usd: Decimal,
    pub current_value_usd: Decimal,
    pub signal_id: String,
    pub exit_reason: Option<String>,
}
impl Position {
    pub fn position_id_or_new(&self) -> String {
        self.position_id.clone().unwrap_or_default()
    }
    pub fn is_open(&self) -> bool {
        self.state == PositionState::Open
    }
    /// Remaining quantity as Decimal atomic units; exits are refused for
    /// positions without trusted integer remaining quantities.
    pub fn trusted_remaining(&self) -> Option<u64> {
        if !self.is_open() {
            return None;
        }
        if !self.reconciliation_status.quantity_is_trusted() {
            return None;
        }
        self.remaining_quantity_atomic
    }
}
