//! Position accounting. Token and base-mint quantities are atomic integers;
//! Decimal appears only for USD values. Realized PnL is derived from verified
//! fill values, never from quote expectations or floats.
use crate::{
    domain::{
        position::{Position, PositionState, ReconciliationStatus},
        trade::Fill,
    },
    economics::CostModel,
};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub struct ExitOutcome {
    pub realized_pnl_usd: Decimal,
    pub closed: bool,
}

#[derive(Default)]
pub struct Portfolio {
    positions: HashMap<String, Position>,
}
impl Portfolio {
    pub fn position(&self, mint: &str) -> Option<&Position> {
        self.positions.get(mint)
    }
    pub fn positions(&self) -> impl Iterator<Item = &Position> {
        self.positions.values()
    }
    pub fn open_positions(&self) -> Vec<&Position> {
        self.positions.values().filter(|p| p.is_open()).collect()
    }
    pub fn load(&mut self, stored: Vec<Position>) {
        for p in stored {
            self.positions.insert(p.mint.clone(), p);
        }
    }
    /// Syncs a single position from external (authoritative) data.
    /// Used to ensure in-memory state matches the persistent store before
    /// applying accounting changes.
    pub fn sync_position(&mut self, position: Position) {
        self.positions.insert(position.mint.clone(), position);
    }

    /// Records a verified entry fill. `price` is the fill's verified execution
    /// price; the USD cost basis comes from `fill.input_value_usd`.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_entry(
        &mut self,
        mint: String,
        base_mint: String,
        token_decimals: u8,
        base_mint_decimals: u8,
        position_id: String,
        fill: &Fill,
        price: Decimal,
        signal_id: String,
        cost_model: CostModel,
    ) -> Result<(), String> {
        if fill.output_amount == 0 {
            return Err("zero fill".into());
        }
        let Some(input_value_usd) = fill.input_value_usd else {
            return Err("entry fill lacks USD value basis".into());
        };
        let q = Decimal::from(fill.output_amount);
        let remaining = fill.output_amount;
        let base_units =
            Decimal::from(fill.input_amount) / Decimal::from(10u64.pow(base_mint_decimals as u32));
        let base_entry_price_usd = if base_units > Decimal::ZERO {
            Some(input_value_usd / base_units)
        } else {
            None
        };
        self.positions
            .entry(mint.clone())
            .and_modify(|p| {
                let total = p.quantity + q;
                p.entry_price_usd = (p.entry_price_usd * p.quantity + price * q) / total;
                p.quantity = total;
                p.remaining_quantity_atomic = Some(
                    p.remaining_quantity_atomic
                        .unwrap_or(0)
                        .saturating_add(remaining),
                );
                p.entry_cost_usd =
                    Some(p.entry_cost_usd.unwrap_or(Decimal::ZERO) + input_value_usd);
                p.high_water_price_usd = p.high_water_price_usd.max(price);
                p.fees_usd += fill.fees_usd;
                p.entry_fees_usd = Some(p.entry_fees_usd.unwrap_or(Decimal::ZERO) + fill.fees_usd);
                p.current_value_usd = price * total;
                p.unrealized_pnl_usd = (price - p.entry_price_usd) * total - p.fees_usd;
                p.entry_input_amount_atomic = p
                    .entry_input_amount_atomic
                    .and_then(|v| v.checked_add(fill.input_amount));
                p.entry_output_amount_atomic = p
                    .entry_output_amount_atomic
                    .and_then(|v| v.checked_add(fill.output_amount));
                p.entry_slippage_bps = Some(
                    p.entry_slippage_bps
                        .unwrap_or_default()
                        .max(fill.slippage_bps),
                );
                p.state = PositionState::Open;
                p.reconciliation_status = ReconciliationStatus::AdjustedOnChain;
            })
            .or_insert(Position {
                mint: mint.clone(),
                position_id: Some(position_id),
                token_mint: Some(mint),
                base_mint: Some(base_mint),
                entry_input_amount_atomic: Some(fill.input_amount),
                entry_output_amount_atomic: Some(fill.output_amount),
                token_decimals: Some(token_decimals),
                base_mint_decimals: Some(base_mint_decimals),
                entry_fees_usd: Some(fill.fees_usd),
                entry_slippage_bps: Some(fill.slippage_bps),
                entry_cost_model: Some(cost_model),
                quantity: q,
                remaining_quantity_atomic: Some(remaining),
                entry_cost_usd: Some(input_value_usd),
                base_entry_price_usd,
                state: PositionState::Open,
                reconciliation_status: ReconciliationStatus::Reconciled,
                last_reconciled_at: Some(fill.confirmed_at),
                exit_signature: None,
                exit_fees_usd: None,
                exit_time: None,
                entry_price_usd: price,
                entry_time: fill.confirmed_at,
                entry_signature: fill.signature.clone(),
                high_water_price_usd: price,
                realized_pnl_usd: Decimal::ZERO,
                unrealized_pnl_usd: -fill.fees_usd,
                fees_usd: fill.fees_usd,
                current_value_usd: price * q,
                signal_id,
                exit_reason: None,
            });
        Ok(())
    }

    /// Applies a verified exit fill against trusted integer remaining quantity.
    /// Selling an assumed quantity is impossible: an unknown remaining balance
    /// is a hard error.
    pub fn apply_exit(
        &mut self,
        mint: &str,
        fill: &Fill,
        exit_reason: &str,
        now: DateTime<Utc>,
    ) -> Result<ExitOutcome, String> {
        let p = self
            .positions
            .get_mut(mint)
            .ok_or_else(|| "no position for mint".to_string())?;
        let Some(remaining) = p.remaining_quantity_atomic else {
            return Err("remaining quantity unknown; refusing exit accounting".into());
        };
        let sold = fill.input_amount;
        let new_remaining = remaining
            .checked_sub(sold)
            .ok_or_else(|| format!("exit sold {sold} exceeds remaining {remaining}"))?;
        let Some(proceeds) = fill.input_value_usd else {
            return Err("exit fill lacks USD proceeds".into());
        };
        let Some(entry_cost) = p.entry_cost_usd else {
            return Err("position lacks entry cost basis".into());
        };
        if p.quantity == Decimal::ZERO {
            return Err("position has zero entry quantity".into());
        }
        let cost_of_sold = entry_cost * Decimal::from(sold) / p.quantity;
        let realized = proceeds - cost_of_sold - fill.fees_usd;
        p.remaining_quantity_atomic = Some(new_remaining);
        p.realized_pnl_usd += realized;
        p.fees_usd += fill.fees_usd;
        p.exit_fees_usd = Some(p.exit_fees_usd.unwrap_or(Decimal::ZERO) + fill.fees_usd);
        p.high_water_price_usd = p.high_water_price_usd.max(fill.price_usd);
        p.current_value_usd = fill.price_usd * Decimal::from(new_remaining);
        let closed = new_remaining == 0;
        if closed {
            p.state = PositionState::Closed;
            p.exit_reason = Some(exit_reason.to_string());
            p.exit_signature = Some(fill.signature.clone());
            p.exit_time = Some(now);
            p.unrealized_pnl_usd = Decimal::ZERO;
        }
        Ok(ExitOutcome {
            realized_pnl_usd: realized,
            closed,
        })
    }

    /// Marks an open position to a current price and maintains the trailing
    /// high-water mark.
    pub fn mark_to_market(&mut self, mint: &str, price: Decimal) -> Result<Decimal, String> {
        let p = self
            .positions
            .get_mut(mint)
            .ok_or_else(|| "no position for mint".to_string())?;
        let remaining = p.remaining_quantity_atomic.unwrap_or(0);
        p.high_water_price_usd = p.high_water_price_usd.max(price);
        p.current_value_usd = price * Decimal::from(remaining);
        let cost_of_remaining = p.entry_cost_usd.unwrap_or(Decimal::ZERO)
            * Decimal::from(remaining)
            / if p.quantity == Decimal::ZERO {
                Decimal::ONE
            } else {
                p.quantity
            };
        p.unrealized_pnl_usd = p.current_value_usd - cost_of_remaining - p.fees_usd;
        Ok(p.current_value_usd)
    }

    /// Restarts must reconcile against the wallet: an on-chain balance that
    /// differs from internal remaining quantity is adopted (the chain is
    /// authoritative) and flagged; positions whose mint vanished are closed.
    pub fn reconcile_with_chain(
        &mut self,
        mint: &str,
        on_chain_amount: u64,
        decimals: u8,
        now: DateTime<Utc>,
    ) -> Result<bool, String> {
        let Some(p) = self.positions.get_mut(mint) else {
            return Ok(false);
        };
        if !p.is_open() {
            return Ok(false);
        }
        if p.remaining_quantity_atomic == Some(on_chain_amount) {
            p.reconciliation_status = ReconciliationStatus::Reconciled;
            p.last_reconciled_at = Some(now);
            return Ok(true);
        }
        if on_chain_amount == 0 {
            p.state = PositionState::Closed;
            p.reconciliation_status = ReconciliationStatus::ClosedOnChain;
            p.last_reconciled_at = Some(now);
            p.exit_reason = p
                .exit_reason
                .clone()
                .or_else(|| Some("reconciled: no on-chain balance".into()));
            p.current_value_usd = Decimal::ZERO;
            p.unrealized_pnl_usd = Decimal::ZERO;
            return Ok(true);
        }
        // Never invent the difference: adopt the chain amount but flag it, and
        // require decimals to match what was recorded at entry.
        if p.token_decimals.is_none() {
            p.token_decimals = Some(decimals);
        } else if p.token_decimals != Some(decimals) {
            return Err(format!(
                "decimals mismatch for {mint}: recorded {:?}, chain {decimals}",
                p.token_decimals
            ));
        }
        p.remaining_quantity_atomic = Some(on_chain_amount);
        p.reconciliation_status = ReconciliationStatus::AdjustedOnChain;
        p.last_reconciled_at = Some(now);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::economics::BreakEvenInputs;
    use chrono::Utc;
    use rust_decimal_macros::dec;

    fn cost_model() -> CostModel {
        CostModel {
            observed_at: Utc::now(),
            source: "test".into(),
            is_live_snapshot: false,
            input: BreakEvenInputs {
                position_size_usd: dec!(10),
                avg_priority_fee_usd: Decimal::ZERO,
                avg_swap_fee_bps: Decimal::ZERO,
                avg_slippage_bps: Decimal::ZERO,
                avg_price_impact_bps: Decimal::ZERO,
                failed_tx_rate: Decimal::ZERO,
                avg_failed_tx_cost_usd: Decimal::ZERO,
                assumed_win_loss_ratio: dec!(1),
                assumed_avg_loss_pct: dec!(1),
            },
        }
    }
    fn fill(input_amount: u64, output_amount: u64, value: Decimal, price: Decimal) -> Fill {
        Fill {
            order_id: "o".into(),
            signature: "sig".into(),
            input_amount,
            output_amount,
            price_usd: price,
            fees_usd: Decimal::ZERO,
            slippage_bps: 0,
            confirmed_at: Utc::now(),
            latency_ms: 0,
            fee_lamports: 0,
            input_value_usd: Some(value),
            expected_output_amount: Some(output_amount),
        }
    }
    fn enter(portfolio: &mut Portfolio, input_value: Decimal, out: u64) {
        let f = fill(
            1_000_000,
            out,
            input_value,
            input_value / Decimal::from(out),
        );
        portfolio
            .apply_entry(
                "T".into(),
                "B".into(),
                6,
                9,
                "pos-1".into(),
                &f,
                input_value / Decimal::from(out),
                "sig-1".into(),
                cost_model(),
            )
            .unwrap();
    }
    #[test]
    fn entry_records_integer_and_usd_basis() {
        let mut pf = Portfolio::default();
        enter(&mut pf, dec!(10), 5_000_000);
        let p = pf.position("T").unwrap();
        assert_eq!(p.remaining_quantity_atomic, Some(5_000_000));
        assert_eq!(p.entry_cost_usd, Some(dec!(10)));
        assert_eq!(p.entry_price_usd, dec!(0.000002));
        assert!(p.reconciliation_status.quantity_is_trusted());
    }
    #[test]
    fn full_exit_realizes_pnl_and_closes() {
        let mut pf = Portfolio::default();
        enter(&mut pf, dec!(10), 5_000_000);
        let exit = fill(
            5_000_000,
            200_000_000,
            dec!(11),
            dec!(11) / Decimal::from(5_000_000),
        );
        let out = pf
            .apply_exit("T", &exit, "take_profit", Utc::now())
            .unwrap();
        assert!(out.closed);
        assert_eq!(out.realized_pnl_usd, dec!(1));
        let p = pf.position("T").unwrap();
        assert_eq!(p.state, PositionState::Closed);
        assert_eq!(p.realized_pnl_usd, dec!(1));
        assert_eq!(p.exit_signature.as_deref(), Some("sig"));
        assert_eq!(p.remaining_quantity_atomic, Some(0));
    }
    #[test]
    fn partial_exit_then_oversell_is_refused() {
        let mut pf = Portfolio::default();
        enter(&mut pf, dec!(10), 5_000_000);
        let half = fill(2_500_000, 100_000_000, dec!(5), dec!(0.000002));
        let out = pf.apply_exit("T", &half, "risk", Utc::now()).unwrap();
        assert!(!out.closed);
        assert_eq!(out.realized_pnl_usd, Decimal::ZERO);
        let over = fill(3_000_000, 100_000_000, dec!(5), dec!(0.000002));
        assert!(pf.apply_exit("T", &over, "risk", Utc::now()).is_err());
        assert_eq!(
            pf.position("T").unwrap().remaining_quantity_atomic,
            Some(2_500_000)
        );
    }
    #[test]
    fn exit_with_fees_reduces_realized_pnl() {
        let mut pf = Portfolio::default();
        enter(&mut pf, dec!(10), 5_000_000);
        let mut exit = fill(5_000_000, 200_000_000, dec!(11), dec!(0.0000022));
        exit.fees_usd = dec!(0.4);
        let out = pf.apply_exit("T", &exit, "stop", Utc::now()).unwrap();
        assert_eq!(out.realized_pnl_usd, dec!(0.6));
    }
    #[test]
    fn unknown_remaining_blocks_exit_accounting() {
        let mut pf = Portfolio::default();
        enter(&mut pf, dec!(10), 5_000_000);
        pf.positions.get_mut("T").unwrap().remaining_quantity_atomic = None;
        let exit = fill(1, 1, dec!(1), dec!(1));
        assert!(pf.apply_exit("T", &exit, "stop", Utc::now()).is_err());
    }
    #[test]
    fn chain_reconciliation_adopts_on_chain_truth_and_flags() {
        let mut pf = Portfolio::default();
        enter(&mut pf, dec!(10), 5_000_000);
        assert!(pf
            .reconcile_with_chain("T", 4_000_000, 6, Utc::now())
            .unwrap());
        let p = pf.position("T").unwrap();
        assert_eq!(p.remaining_quantity_atomic, Some(4_000_000));
        assert_eq!(
            p.reconciliation_status,
            ReconciliationStatus::AdjustedOnChain
        );
        assert!(pf.reconcile_with_chain("T", 0, 6, Utc::now()).unwrap());
        assert_eq!(pf.position("T").unwrap().state, PositionState::Closed);
    }
    #[test]
    fn decimals_mismatch_is_an_error_not_a_guess() {
        let mut pf = Portfolio::default();
        enter(&mut pf, dec!(10), 5_000_000);
        assert!(pf
            .reconcile_with_chain("T", 4_000_000, 9, Utc::now())
            .is_err());
    }
    #[test]
    fn mark_to_market_tracks_high_water() {
        let mut pf = Portfolio::default();
        enter(&mut pf, dec!(10), 5_000_000);
        pf.mark_to_market("T", dec!(0.000003)).unwrap();
        let p = pf.position("T").unwrap();
        assert_eq!(p.high_water_price_usd, dec!(0.000003));
        assert_eq!(p.current_value_usd, dec!(15));
        assert_eq!(p.unrealized_pnl_usd, dec!(5));
    }
    #[test]
    fn legacy_position_with_no_remaining_is_not_open_for_exits() {
        let mut pf = Portfolio::default();
        pf.load(vec![serde_json::from_str(&serde_json::json!({"mint":"legacy","quantity":"1","entry_price_usd":"1","entry_time":Utc::now(),"entry_signature":"sig","high_water_price_usd":"1","realized_pnl_usd":"0","unrealized_pnl_usd":"0","fees_usd":"0","current_value_usd":"1","signal_id":"signal","exit_reason":null}).to_string()).unwrap()]);
        let p = pf.position("legacy").unwrap();
        assert!(p.is_open());
        assert!(p.trusted_remaining().is_none());
    }
}
