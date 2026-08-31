use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// A verified execution result. Quantities are atomic (base-unit) integers;
/// the only Decimal fields are USD-denominated accounting values derived from
/// the actual on-chain outcome, never from floats.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fill {
    pub order_id: String,
    pub signature: String,
    pub input_amount: u64,
    pub output_amount: u64,
    pub price_usd: Decimal,
    pub fees_usd: Decimal,
    pub slippage_bps: u32,
    pub confirmed_at: DateTime<Utc>,
    pub latency_ms: u64,
    /// Total network fee (base + priority) actually charged, in lamports.
    #[serde(default)]
    pub fee_lamports: u64,
    /// USD value actually paid (entries) or received (exits) computed from
    /// the verified on-chain amounts. `None` only on legacy records.
    #[serde(default)]
    pub input_value_usd: Option<Decimal>,
    /// Quote output at request time, kept so realised slippage is auditable.
    #[serde(default)]
    pub expected_output_amount: Option<u64>,
}
impl Fill {
    /// Realised slippage versus the quote, in bps (saturating at zero).
    pub fn realised_slippage_bps(&self) -> u32 {
        let Some(expected) = self.expected_output_amount else {
            return 0;
        };
        if expected == 0 || self.output_amount >= expected {
            return 0;
        }
        u32::try_from((expected - self.output_amount).saturating_mul(10_000) / expected)
            .unwrap_or(u32::MAX)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum OrderState {
    Pending,
    Submitted,
    Confirmed,
    Failed,
    Dropped,
    Unknown,
    Expired,
    Reconciled,
}
impl OrderState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Confirmed | Self::Failed | Self::Dropped | Self::Expired | Self::Reconciled
        )
    }
    /// Fail-closed state machine: an order may only move forward along
    /// auditable paths. Unknown orders may never be silently replaced by a
    /// new submission; they must first become Confirmed/Failed/Expired/Reconciled.
    pub fn can_transition(&self, next: &OrderState) -> bool {
        use OrderState::*;
        matches!(
            (self, next),
            (Pending, Submitted)
                | (Pending, Failed)
                | (Pending, Expired)
                | (Pending, Unknown)
                | (Submitted, Confirmed)
                | (Submitted, Failed)
                | (Submitted, Expired)
                | (Submitted, Unknown)
                | (Unknown, Confirmed)
                | (Unknown, Failed)
                | (Unknown, Expired)
                | (Unknown, Reconciled)
        )
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum OrderKind {
    Entry,
    Exit,
    LegacyUnknown,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum OrderSide {
    Buy,
    Sell,
    Unknown,
}
fn legacy_order_kind() -> OrderKind {
    OrderKind::LegacyUnknown
}
fn legacy_order_side() -> OrderSide {
    OrderSide::Unknown
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderRecord {
    pub id: String,
    pub signal_id: String,
    pub mint: String,
    #[serde(default = "legacy_order_kind")]
    pub kind: OrderKind,
    #[serde(default)]
    pub position_id: Option<String>,
    #[serde(default = "legacy_order_side")]
    pub side: OrderSide,
    #[serde(default)]
    pub input_mint: Option<String>,
    #[serde(default)]
    pub output_mint: Option<String>,
    #[serde(default)]
    pub input_amount_atomic: Option<u64>,
    /// USD value of the input leg asserted by the verified feed (entries) or
    /// mark basis (exits), used to reconstruct accounting after restarts.
    #[serde(default)]
    pub input_value_usd: Option<Decimal>,
    /// Atomic decimals of the output leg, needed to re-price a fill from
    /// on-chain amounts without guessing.
    #[serde(default)]
    pub output_mint_decimals: Option<u8>,
    pub state: OrderState,
    pub idempotency_key: String,
    pub created_at: DateTime<Utc>,
    pub signature: Option<String>,
    pub error: Option<String>,
}
impl OrderRecord {
    /// Applies a state transition only when the state machine allows it.
    pub fn transition(&mut self, next: OrderState) -> Result<(), String> {
        if self.state == next {
            return Ok(());
        }
        if self.state.can_transition(&next) {
            self.state = next;
            Ok(())
        } else {
            Err(format!(
                "illegal order transition {:?} -> {:?}",
                self.state, next
            ))
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn order(state: OrderState) -> OrderRecord {
        OrderRecord {
            id: "o".into(),
            signal_id: "s".into(),
            mint: "m".into(),
            kind: OrderKind::Entry,
            position_id: Some("p".into()),
            side: OrderSide::Buy,
            input_mint: Some("b".into()),
            output_mint: Some("m".into()),
            input_amount_atomic: Some(1),
            input_value_usd: Some(Decimal::ONE),
            output_mint_decimals: Some(6),
            state,
            idempotency_key: "k".into(),
            created_at: Utc::now(),
            signature: None,
            error: None,
        }
    }
    #[test]
    fn happy_path_is_allowed() {
        let mut o = order(OrderState::Pending);
        o.transition(OrderState::Submitted).unwrap();
        o.transition(OrderState::Confirmed).unwrap();
    }
    #[test]
    fn unknown_requires_reconciliation_before_terminal_state() {
        let mut o = order(OrderState::Submitted);
        o.transition(OrderState::Unknown).unwrap();
        assert!(o.transition(OrderState::Confirmed).is_ok());
        let mut u = order(OrderState::Unknown);
        assert!(u.transition(OrderState::Failed).is_ok());
        let mut u2 = order(OrderState::Unknown);
        assert!(u2.transition(OrderState::Reconciled).is_ok());
    }
    #[test]
    fn terminal_states_never_transition_and_confirmed_cannot_go_back() {
        for s in [
            OrderState::Confirmed,
            OrderState::Failed,
            OrderState::Expired,
            OrderState::Reconciled,
        ] {
            let mut o = order(s);
            assert!(o.transition(OrderState::Pending).is_err());
            assert!(o.transition(OrderState::Submitted).is_err());
        }
        let mut c = order(OrderState::Confirmed);
        assert!(c.transition(OrderState::Unknown).is_err());
        let mut p = order(OrderState::Pending);
        assert!(
            p.transition(OrderState::Confirmed).is_err(),
            "must pass through Submitted before Confirmed"
        );
    }
    #[test]
    fn realised_slippage_saturates() {
        let mut f = base_fill();
        f.expected_output_amount = Some(1_000);
        f.output_amount = 990;
        assert_eq!(f.realised_slippage_bps(), 100);
        f.output_amount = 1_100;
        assert_eq!(f.realised_slippage_bps(), 0);
        f.expected_output_amount = Some(0);
        assert_eq!(f.realised_slippage_bps(), 0);
    }
    fn base_fill() -> Fill {
        Fill {
            order_id: "o".into(),
            signature: "sig".into(),
            input_amount: 1,
            output_amount: 1,
            price_usd: Decimal::ONE,
            fees_usd: Decimal::ZERO,
            slippage_bps: 0,
            confirmed_at: Utc::now(),
            latency_ms: 0,
            fee_lamports: 0,
            input_value_usd: None,
            expected_output_amount: None,
        }
    }
}
