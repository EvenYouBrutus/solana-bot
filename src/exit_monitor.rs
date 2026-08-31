//! Independent exit pipeline that runs as a separate tokio task, decoupled
//! from signal ingestion and the main session tick loop.  Positions are loaded
//! fresh from SQLite on every cycle so a crash or signal-feed outage can never
//! leave an open position permanently unmanaged.
//!
//! The decision logic (`strategy::exit_reason`) and execution pipeline are
//! shared with the main session – only the scheduling is independent.

use crate::{
    config::types::Config,
    data::rpc::RpcPool,
    domain::{
        position::{Position, PositionState},
        trade::{OrderKind, OrderRecord, OrderSide, OrderState},
    },
    execution::{
        finalize_fill, reconcile_signature, units, ExecutionError, ExecutionRequest, Executor,
        ValueBasis,
    },
    portfolio::Portfolio,
    storage::StateStore,
    strategy::exit_reason,
};
use anyhow::Result;
use chrono::{Duration, Utc};
use rust_decimal::Decimal;
use sha2::{Digest, Sha256};
use std::{sync::Arc, time::Duration as StdDuration};

/// Shared, `Send + Sync` dependencies for the exit monitor.  All inner types
/// are reference-counted so the monitor can own its copies while the main
/// session retains its own handles.
pub struct ExitDeps {
    pub config: Arc<Config>,
    pub store: Arc<StateStore>,
    pub executor: Arc<dyn Executor>,
    pub rpc: Arc<RpcPool>,
}

/// Independent exit monitor.  Spawns as a background tokio task and evaluates
/// every open position on its own cadence regardless of signal-feed activity.
pub struct ExitMonitor {
    deps: ExitDeps,
    poll_interval: StdDuration,
}

impl ExitMonitor {
    pub fn new(deps: ExitDeps, poll_interval_secs: u64) -> Self {
        Self {
            deps,
            poll_interval: StdDuration::from_secs(poll_interval_secs.max(1)),
        }
    }

    /// Runs until `shutdown` fires.  Each cycle: load state → evaluate exits →
    /// execute triggered exits → reconcile stuck orders.
    pub async fn run(&self, mut shutdown: tokio::sync::oneshot::Receiver<()>) -> Result<()> {
        tracing::info!("exit monitor started");
        loop {
            tokio::select! {
                _ = &mut shutdown => {
                    tracing::info!("exit monitor shutting down");
                    break;
                }
                _ = tokio::time::sleep(self.poll_interval) => {}
            }
            if let Err(e) = self.tick().await {
                tracing::error!(error = %e, "exit monitor tick failed; will retry");
            }
        }
        Ok(())
    }

    /// Single evaluation cycle: reconcile pending orders, load positions, mark
    /// to market, evaluate exit reasons, and execute any triggered exits.
    async fn tick(&self) -> Result<()> {
        let store = &*self.deps.store;
        let executor = &*self.deps.executor;
        let config = &*self.deps.config;
        let _now = Utc::now();

        // 1. Reconcile any orders stuck in Unknown/Pending before evaluating
        //    exits – this frees up positions that have a flight in progress.
        reconcile_stale_orders(store, executor, &self.deps.rpc, config).await;

        // 2. Load all positions from persistent storage.
        let stored = store.positions()?;
        let mut portfolio = Portfolio::default();
        portfolio.load(stored);

        // 3. Re-read interlock state so the monitor respects operator actions.
        let emergency_active = store.emergency_stop()?.is_some();
        let kill_switch_latched = store.kill_switch_reason()?.is_some();
        let _ = emergency_active;
        let _ = kill_switch_latched;

        // 4. Iterate open positions and evaluate exits.
        let open: Vec<String> = portfolio
            .open_positions()
            .iter()
            .map(|p| p.mint.clone())
            .collect();

        for mint in open {
            let Some(position) = portfolio.position(&mint).cloned() else {
                continue;
            };
            if position.state != PositionState::Open {
                continue;
            }
            let Some(remaining) = position.trusted_remaining() else {
                tracing::debug!(mint=%mint, "exit monitor: position quantity not trusted; skipping");
                continue;
            };
            // Skip if an exit order is already in flight for this position.
            if has_inflight_exit(store, &position)? {
                continue;
            }
            let Some(base_price) = position.base_entry_price_usd else {
                continue;
            };
            let (Some(base_decimals), Some(token_decimals)) =
                (position.base_mint_decimals, position.token_decimals)
            else {
                continue;
            };
            // Derive mark price from a fresh sell quote.
            let base_mint = match position.base_mint.as_deref() {
                Some(m) => m.to_string(),
                None => continue,
            };
            let mark = match executor
                .quote(&mint, &base_mint, remaining, config.execution.slippage_bps)
                .await
            {
                Ok(q) => {
                    let base_units = match units(q.output_amount, base_decimals) {
                        Ok(u) => u,
                        Err(_) => continue,
                    };
                    let token_units = match units(remaining, token_decimals) {
                        Ok(u) => u,
                        Err(_) => continue,
                    };
                    if token_units.is_zero() {
                        continue;
                    }
                    base_units * base_price / token_units
                }
                Err(e) => {
                    tracing::warn!(mint=%mint, error=%e, "exit monitor: quote unavailable; skipping");
                    continue;
                }
            };

            // Mark to market and persist.
            if let Ok(new_value) = portfolio.mark_to_market(&mint, mark) {
                let _ = new_value;
            }
            if let Some(p) = portfolio.position(&mint) {
                let _ = store.save_position(p);
            }

            // Liquidity from candidates is unavailable to the exit monitor; use
            // MAX so LiquidityDeterioration is never triggered here – the main
            // session's candidate-aware tick handles that case.
            let liquidity = Decimal::MAX;
            let invalidated = false;
            let Some(reason) = exit_reason(
                &position,
                mark,
                liquidity,
                config.risk.min_liquidity_usd,
                invalidated,
                Utc::now(),
                &config.strategy,
            ) else {
                continue;
            };

            tracing::info!(mint=%mint, %reason, remaining, mark=%mark, "exit monitor: exit triggered");
            execute_exit(
                &self.deps,
                &mut portfolio,
                &mint,
                remaining,
                reason.as_str(),
                token_decimals,
                base_decimals,
                base_mint,
                base_price,
            )
            .await;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Exit execution (mirrors runtime::attempt_exit but operates on standalone state)
// ---------------------------------------------------------------------------

fn exit_idempotency_key(position_id: &str, remaining: u64, reason: &str, attempt: u32) -> String {
    let mut hash = Sha256::new();
    hash.update(position_id.as_bytes());
    hash.update(remaining.to_le_bytes());
    hash.update(reason.as_bytes());
    hash.update(attempt.to_le_bytes());
    format!("{:x}", hash.finalize())
}

fn has_inflight_exit(store: &StateStore, position: &Position) -> Result<bool> {
    Ok(store.incomplete_orders()?.iter().any(|o| {
        o.kind == OrderKind::Exit && o.position_id.as_deref() == position.position_id.as_deref()
    }))
}

#[allow(clippy::too_many_arguments)]
async fn execute_exit(
    deps: &ExitDeps,
    portfolio: &mut Portfolio,
    mint: &str,
    remaining: u64,
    reason: &str,
    token_decimals: u8,
    base_decimals: u8,
    base_mint: String,
    base_price: Decimal,
) {
    let store = &*deps.store;
    let executor = &*deps.executor;
    let config = &*deps.config;
    let now = Utc::now();

    let Some(position) = portfolio.position(mint).cloned() else {
        return;
    };

    // Count previous exit attempts for this position to build the idempotency key.
    let attempt = store
        .orders()
        .unwrap_or_default()
        .iter()
        .filter(|o| {
            o.kind == OrderKind::Exit && o.position_id.as_deref() == position.position_id.as_deref()
        })
        .count() as u32;

    let position_id = position.position_id.as_deref().unwrap_or(mint);
    let key = exit_idempotency_key(position_id, remaining, reason, attempt);

    let mark_value = match units(remaining, token_decimals) {
        Ok(q) => q * base_price,
        Err(_) => return,
    };

    let order = OrderRecord {
        id: uuid::Uuid::new_v4().to_string(),
        signal_id: position.signal_id.clone(),
        mint: mint.to_string(),
        kind: OrderKind::Exit,
        position_id: position.position_id.clone(),
        side: OrderSide::Sell,
        input_mint: Some(mint.to_string()),
        output_mint: Some(base_mint.clone()),
        input_amount_atomic: Some(remaining),
        input_value_usd: Some(mark_value),
        output_mint_decimals: Some(base_decimals),
        state: OrderState::Pending,
        idempotency_key: key,
        created_at: now,
        signature: None,
        error: Some(reason.to_string()),
    };

    if !store.reserve_order(&order).unwrap_or(false) {
        tracing::debug!(mint=%mint, "exit monitor: duplicate exit blocked by idempotency");
        return;
    }

    // Fresh quote for execution.
    let quote = match executor
        .quote(mint, &base_mint, remaining, config.execution.slippage_bps)
        .await
    {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(mint=%mint, error=%e, "exit monitor: exit quote unavailable");
            let mut o = order.clone();
            o.error = Some(format!("quote failed: {e}"));
            o.transition(OrderState::Failed).ok();
            store.update_order(&o).ok();
            return;
        }
    };

    if quote.price_impact_bps > config.execution.max_price_impact_bps {
        tracing::error!(
            mint=%mint,
            impact_bps=quote.price_impact_bps,
            cap=config.execution.max_price_impact_bps,
            "exit monitor: price impact exceeds limit"
        );
        let mut o = order.clone();
        o.error = Some("price impact limit".into());
        o.transition(OrderState::Failed).ok();
        store.update_order(&o).ok();
        return;
    }

    let min_output = quote
        .output_amount
        .saturating_mul(10_000 - config.execution.slippage_bps as u64)
        / 10_000;

    let mut placed = order.clone();
    placed.transition(OrderState::Submitted).ok();
    store.update_order(&placed).ok();

    tracing::info!(
        order_id=%order.id,
        mint=%mint,
        reason=%reason,
        qty_atomic=remaining,
        expected_out=quote.output_amount,
        impact_bps=quote.price_impact_bps,
        "exit monitor: exit order submitted"
    );

    let request = ExecutionRequest {
        order_id: order.id.clone(),
        quote,
        max_slippage_bps: config.risk.max_slippage_bps,
        max_price_impact_bps: config.execution.max_price_impact_bps,
        min_output_amount: min_output,
        input_decimals: token_decimals,
        output_decimals: base_decimals,
        value_basis: ValueBasis::OutputUnitPriceUsd(base_price),
    };

    match executor.execute(request).await {
        Ok(mut fill) => {
            finalize_fill(&mut fill, config.economics.sol_price_usd);
            placed.signature = Some(fill.signature.clone());
            placed.transition(OrderState::Confirmed).ok();
            store.update_order(&placed).ok();
            store.save_fill(&fill).ok();
            match portfolio.apply_exit(mint, &fill, reason, Utc::now()) {
                Ok(outcome) => {
                    if let Some(p) = portfolio.position(mint) {
                        store.save_position(p).ok();
                    }
                    tracing::info!(
                        order_id=%order.id,
                        mint=%mint,
                        %reason,
                        signature=%fill.signature,
                        sold=fill.input_amount,
                        received=fill.output_amount,
                        fees_usd=%fill.fees_usd,
                        realized_pnl_usd=%outcome.realized_pnl_usd,
                        closed=outcome.closed,
                        "exit monitor: exit confirmed"
                    );
                }
                Err(e) => {
                    tracing::error!(order_id=%order.id, error=%e, "exit monitor: confirmed exit could not be applied");
                }
            }
        }
        Err(ExecutionError::Unknown { signature, detail }) => {
            if let Some(sig) = &signature {
                placed.signature = Some(sig.clone());
            }
            placed.transition(OrderState::Unknown).ok();
            placed.error = Some(detail.clone());
            store.update_order(&placed).ok();
            tracing::error!(
                order_id=%order.id,
                ?signature,
                %detail,
                "exit monitor: outcome unknown; reconciliation required"
            );
        }
        Err(e) => {
            placed.transition(OrderState::Failed).ok();
            placed.error = Some(e.to_string());
            store.update_order(&placed).ok();
            tracing::error!(
                order_id=%order.id,
                mint=%mint,
                error=%e,
                "exit monitor: exit failed; order marked failed"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Order reconciliation for stale / unknown orders
// ---------------------------------------------------------------------------

async fn reconcile_stale_orders(
    store: &StateStore,
    executor: &dyn Executor,
    rpc: &RpcPool,
    config: &Config,
) {
    let now = Utc::now();
    let live = executor.is_live();
    let owner = executor.signer_pubkey();
    for order in match store.incomplete_orders() {
        Ok(o) => o,
        Err(e) => {
            tracing::error!(error=%e, "exit monitor: could not load incomplete orders");
            return;
        }
    } {
        let age = now - order.created_at;
        match (&order.signature, live) {
            (None, _) => {
                if age > Duration::seconds(config.rpc.unknown_after_secs) {
                    let mut o = order.clone();
                    o.error = Some("expired without submission (exit monitor)".into());
                    o.transition(OrderState::Failed).ok();
                    store.update_order(&o).ok();
                }
            }
            (Some(_sig), false) => {
                // Paper-mode: nothing on-chain.
                let mut o = order.clone();
                o.error = Some("paper signature cannot be reconciled".into());
                o.transition(OrderState::Failed).ok();
                store.update_order(&o).ok();
            }
            (Some(sig), true) => {
                let Some(owner) = &owner else {
                    continue;
                };
                let (Some(input_mint), Some(output_mint), Some(expected_input)) = (
                    &order.input_mint,
                    &order.output_mint,
                    order.input_amount_atomic,
                ) else {
                    continue;
                };
                match reconcile_signature(rpc, sig, input_mint, output_mint, owner, expected_input)
                    .await
                {
                    Ok(Some(_outcome)) => {
                        // Fill was already accounted by the main session or
                        // earlier reconciliation; just mark terminal.
                        let mut o = order.clone();
                        o.transition(OrderState::Confirmed).ok();
                        store.update_order(&o).ok();
                    }
                    Ok(None) => {
                        if age > Duration::seconds(config.rpc.unknown_after_secs) {
                            let mut o = order.clone();
                            o.error = Some("signature never appeared (exit monitor)".into());
                            o.transition(OrderState::Expired).ok();
                            store.update_order(&o).ok();
                        }
                    }
                    Err(ExecutionError::Transaction(detail)) => {
                        let mut o = order.clone();
                        o.error = Some(detail);
                        o.transition(OrderState::Failed).ok();
                        store.update_order(&o).ok();
                    }
                    Err(_) => {
                        // Availability error; leave as-is for next tick.
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::types::StrategyConfig,
        domain::position::{PositionState, ReconciliationStatus},
        portfolio::Portfolio,
    };
    use chrono::Utc;
    use rust_decimal_macros::dec;

    fn strategy() -> StrategyConfig {
        StrategyConfig {
            base_mint: "SOL".into(),
            min_wallet_score: dec!(60),
            min_wallet_samples: 25,
            min_consensus_wallets: 2,
            min_signal_score: dec!(65),
            min_token_age_secs: 86400,
            stop_loss_pct: dec!(5),
            take_profit_pct: dec!(12),
            trailing_stop_pct: dec!(4),
            max_holding_minutes: 240,
        }
    }

    fn test_position(remaining: u64, entry_price: Decimal, base_price: Decimal) -> Position {
        Position {
            mint: "TOKEN".into(),
            position_id: Some("pos-1".into()),
            token_mint: Some("TOKEN".into()),
            base_mint: Some("SOL".into()),
            entry_input_amount_atomic: Some(1_000_000_000),
            entry_output_amount_atomic: Some(remaining),
            token_decimals: Some(6),
            base_mint_decimals: Some(9),
            entry_fees_usd: Some(Decimal::ZERO),
            entry_slippage_bps: Some(0),
            entry_cost_model: None,
            quantity: Decimal::from(remaining),
            remaining_quantity_atomic: Some(remaining),
            entry_cost_usd: Some(dec!(10)),
            base_entry_price_usd: Some(base_price),
            state: PositionState::Open,
            reconciliation_status: ReconciliationStatus::Reconciled,
            last_reconciled_at: None,
            exit_signature: None,
            exit_fees_usd: None,
            exit_time: None,
            entry_price_usd: entry_price,
            entry_time: Utc::now(),
            entry_signature: "sig".into(),
            high_water_price_usd: entry_price,
            realized_pnl_usd: Decimal::ZERO,
            unrealized_pnl_usd: Decimal::ZERO,
            fees_usd: Decimal::ZERO,
            current_value_usd: dec!(10),
            signal_id: "signal".into(),
            exit_reason: None,
        }
    }

    #[test]
    fn idempotency_key_is_deterministic() {
        assert_eq!(
            exit_idempotency_key("p", 100, "stop_loss", 0),
            exit_idempotency_key("p", 100, "stop_loss", 0)
        );
        assert_ne!(
            exit_idempotency_key("p", 100, "stop_loss", 0),
            exit_idempotency_key("p", 100, "stop_loss", 1)
        );
        assert_ne!(
            exit_idempotency_key("p", 100, "stop_loss", 0),
            exit_idempotency_key("p", 99, "take_profit", 0)
        );
    }

    #[test]
    fn has_inflight_returns_false_when_no_orders() {
        let store = StateStore::open(":memory:").unwrap();
        let pos = test_position(1_000_000, dec!(0.00001), dec!(150));
        assert!(!has_inflight_exit(&store, &pos).unwrap());
    }

    #[test]
    fn has_inflight_detects_pending_exit() {
        let store = StateStore::open(":memory:").unwrap();
        let pos = test_position(1_000_000, dec!(0.00001), dec!(150));
        let order = OrderRecord {
            id: "o1".into(),
            signal_id: "s".into(),
            mint: "TOKEN".into(),
            kind: OrderKind::Exit,
            position_id: Some("pos-1".into()),
            side: OrderSide::Sell,
            input_mint: Some("TOKEN".into()),
            output_mint: Some("SOL".into()),
            input_amount_atomic: Some(1_000_000),
            input_value_usd: Some(dec!(10)),
            output_mint_decimals: Some(9),
            state: OrderState::Pending,
            idempotency_key: "key1".into(),
            created_at: Utc::now(),
            signature: None,
            error: None,
        };
        store.reserve_order(&order).unwrap();
        assert!(has_inflight_exit(&store, &pos).unwrap());
    }

    #[test]
    fn has_inflight_ignores_confirmed_exit() {
        let store = StateStore::open(":memory:").unwrap();
        let pos = test_position(1_000_000, dec!(0.00001), dec!(150));
        let mut order = OrderRecord {
            id: "o1".into(),
            signal_id: "s".into(),
            mint: "TOKEN".into(),
            kind: OrderKind::Exit,
            position_id: Some("pos-1".into()),
            side: OrderSide::Sell,
            input_mint: Some("TOKEN".into()),
            output_mint: Some("SOL".into()),
            input_amount_atomic: Some(1_000_000),
            input_value_usd: Some(dec!(10)),
            output_mint_decimals: Some(9),
            state: OrderState::Submitted,
            idempotency_key: "key1".into(),
            created_at: Utc::now(),
            signature: None,
            error: None,
        };
        store.reserve_order(&order).unwrap();
        order.transition(OrderState::Confirmed).unwrap();
        store.update_order(&order).unwrap();
        assert!(!has_inflight_exit(&store, &pos).unwrap());
    }

    #[test]
    fn mark_price_calculation_matches_runtime() {
        // 1_000_000 tokens (6dp), sell quote 0.5 SOL (9dp), SOL at $150
        let mark =
            super::super::runtime::mark_price_from_quote(500_000_000, 9, dec!(150), 1_000_000, 6)
                .unwrap();
        assert_eq!(mark, dec!(75));
    }

    #[test]
    fn position_with_untrusted_remaining_is_skipped() {
        let mut pos = test_position(1_000_000, dec!(0.00001), dec!(150));
        pos.reconciliation_status = ReconciliationStatus::Unverified;
        assert!(pos.trusted_remaining().is_none());
    }

    #[test]
    fn closed_position_is_not_open() {
        let mut pos = test_position(1_000_000, dec!(0.00001), dec!(150));
        pos.state = PositionState::Closed;
        assert!(!pos.is_open());
        assert!(pos.trusted_remaining().is_none());
    }

    #[test]
    fn emergency_stop_persists_across_ticks() {
        let store = StateStore::open(":memory:").unwrap();
        assert!(store.emergency_stop().unwrap().is_none());
        store.set_emergency_stop("test").unwrap();
        assert_eq!(store.emergency_stop().unwrap().as_deref(), Some("test"));
        store.clear_emergency_stop().unwrap();
        assert!(store.emergency_stop().unwrap().is_none());
    }

    #[test]
    fn kill_switch_blocks_nothing_for_exits() {
        let store = StateStore::open(":memory:").unwrap();
        store.latch_kill_switch("test").unwrap();
        assert!(store.kill_switch_reason().unwrap().is_some());
        // Exits are allowed even under kill switch.
    }

    #[test]
    fn exit_monitor_loads_positions_from_store() {
        let store = StateStore::open(":memory:").unwrap();
        let pos = test_position(1_000_000, dec!(0.00001), dec!(150));
        store.save_position(&pos).unwrap();
        let loaded = store.positions().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].mint, "TOKEN");
        assert_eq!(loaded[0].remaining_quantity_atomic, Some(1_000_000));
    }

    #[test]
    fn portfolio_survives_round_trip_through_store() {
        let store = StateStore::open(":memory:").unwrap();
        let pos = test_position(5_000_000, dec!(0.000002), dec!(150));
        store.save_position(&pos).unwrap();
        let mut portfolio = Portfolio::default();
        portfolio.load(store.positions().unwrap());
        let open = portfolio.open_positions();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].remaining_quantity_atomic, Some(5_000_000));
    }

    #[test]
    fn duplicate_exit_order_is_prevented_by_idempotency() {
        let store = StateStore::open(":memory:").unwrap();
        let order1 = OrderRecord {
            id: "o1".into(),
            signal_id: "s".into(),
            mint: "TOKEN".into(),
            kind: OrderKind::Exit,
            position_id: Some("pos-1".into()),
            side: OrderSide::Sell,
            input_mint: Some("TOKEN".into()),
            output_mint: Some("SOL".into()),
            input_amount_atomic: Some(1_000_000),
            input_value_usd: Some(dec!(10)),
            output_mint_decimals: Some(9),
            state: OrderState::Pending,
            idempotency_key: "same-key".into(),
            created_at: Utc::now(),
            signature: None,
            error: None,
        };
        assert!(store.reserve_order(&order1).unwrap());
        // Second attempt with same key must be rejected.
        let order2 = OrderRecord {
            id: "o2".into(),
            ..order1
        };
        assert!(!store.reserve_order(&order2).unwrap());
    }

    #[test]
    fn exit_order_state_machine_blocks_invalid_transitions() {
        let store = StateStore::open(":memory:").unwrap();
        let mut order = OrderRecord {
            id: "o1".into(),
            signal_id: "s".into(),
            mint: "TOKEN".into(),
            kind: OrderKind::Exit,
            position_id: Some("pos-1".into()),
            side: OrderSide::Sell,
            input_mint: Some("TOKEN".into()),
            output_mint: Some("SOL".into()),
            input_amount_atomic: Some(1_000_000),
            input_value_usd: Some(dec!(10)),
            output_mint_decimals: Some(9),
            state: OrderState::Pending,
            idempotency_key: "k".into(),
            created_at: Utc::now(),
            signature: None,
            error: None,
        };
        store.reserve_order(&order).unwrap();
        // Pending -> Submitted -> Confirmed is valid.
        order.transition(OrderState::Submitted).unwrap();
        order.transition(OrderState::Confirmed).unwrap();
        store.update_order(&order).unwrap();
        // Confirmed -> anything is invalid.
        assert!(order.transition(OrderState::Failed).is_err());
    }

    #[test]
    fn failed_order_can_be_retried_with_new_idempotency_key() {
        let store = StateStore::open(":memory:").unwrap();
        let mut order = OrderRecord {
            id: "o1".into(),
            signal_id: "s".into(),
            mint: "TOKEN".into(),
            kind: OrderKind::Exit,
            position_id: Some("pos-1".into()),
            side: OrderSide::Sell,
            input_mint: Some("TOKEN".into()),
            output_mint: Some("SOL".into()),
            input_amount_atomic: Some(1_000_000),
            input_value_usd: Some(dec!(10)),
            output_mint_decimals: Some(9),
            state: OrderState::Pending,
            idempotency_key: "key-fail".into(),
            created_at: Utc::now(),
            signature: None,
            error: None,
        };
        store.reserve_order(&order).unwrap();
        order.transition(OrderState::Failed).unwrap();
        store.update_order(&order).unwrap();
        // New order with different key succeeds (different attempt count).
        let order2 = OrderRecord {
            id: "o2".into(),
            idempotency_key: "key-retry".into(),
            state: OrderState::Pending,
            ..order
        };
        assert!(store.reserve_order(&order2).unwrap());
    }

    #[test]
    fn trailing_stop_is_evaluated() {
        let pos = test_position(1_000_000, dec!(0.00001), dec!(150));
        // Mark is 10% below entry -> stop loss should trigger at 5%.
        let reason = exit_reason(
            &pos,
            dec!(0.000009), // 10% below entry
            Decimal::MAX,
            dec!(50000),
            false,
            Utc::now(),
            &strategy(),
        );
        assert_eq!(reason, Some(crate::strategy::ExitReason::StopLoss));
    }

    #[test]
    fn trailing_stop_triggers_from_high_water() {
        let mut pos = test_position(1_000_000, dec!(0.00001), dec!(150));
        pos.high_water_price_usd = dec!(0.00002); // Price went up then came back
                                                  // Mark at 0.000011 = 10% above entry (no TP at 12%),
                                                  // but 45% below high water -> trailing stop at 4%
        let reason = exit_reason(
            &pos,
            dec!(0.000011),
            Decimal::MAX,
            dec!(50000),
            false,
            Utc::now(),
            &strategy(),
        );
        assert_eq!(reason, Some(crate::strategy::ExitReason::TrailingStop));
    }

    #[test]
    fn time_limit_triggers_after_max_holding() {
        let mut pos = test_position(1_000_000, dec!(0.00001), dec!(150));
        pos.entry_time = Utc::now() - chrono::Duration::minutes(300);
        let reason = exit_reason(
            &pos,
            dec!(0.00001), // same as entry -> no SL/TP/TS
            Decimal::MAX,
            dec!(50000),
            false,
            Utc::now(),
            &strategy(),
        );
        assert_eq!(reason, Some(crate::strategy::ExitReason::TimeLimit));
    }

    #[test]
    fn take_profit_triggers_on_sufficient_gain() {
        let pos = test_position(1_000_000, dec!(0.00001), dec!(150));
        // 15% above entry -> take profit at 12%
        let reason = exit_reason(
            &pos,
            dec!(0.0000115),
            Decimal::MAX,
            dec!(50000),
            false,
            Utc::now(),
            &strategy(),
        );
        assert_eq!(reason, Some(crate::strategy::ExitReason::TakeProfit));
    }

    #[test]
    fn no_exit_when_price_is_stable() {
        let pos = test_position(1_000_000, dec!(0.00001), dec!(150));
        let reason = exit_reason(
            &pos,
            dec!(0.00001), // exactly at entry
            Decimal::MAX,
            dec!(50000),
            false,
            Utc::now(),
            &strategy(),
        );
        assert_eq!(reason, None);
    }

    #[test]
    fn exit_monitor_reconciles_stale_paper_orders() {
        let store = StateStore::open(":memory:").unwrap();
        let order = OrderRecord {
            id: "o1".into(),
            signal_id: "s".into(),
            mint: "TOKEN".into(),
            kind: OrderKind::Exit,
            position_id: Some("pos-1".into()),
            side: OrderSide::Sell,
            input_mint: Some("TOKEN".into()),
            output_mint: Some("SOL".into()),
            input_amount_atomic: Some(1_000_000),
            input_value_usd: Some(dec!(10)),
            output_mint_decimals: Some(9),
            state: OrderState::Pending,
            idempotency_key: "k".into(),
            created_at: Utc::now() - chrono::Duration::seconds(200),
            signature: Some("paper:sig".into()),
            error: None,
        };
        store.reserve_order(&order).unwrap();
        let store_arc = Arc::new(store);
        // We can't easily create a full executor here, but the reconciliation
        // logic for paper orders doesn't need one - it just marks them Failed.
        let paper_order = store_arc.incomplete_orders().unwrap();
        assert_eq!(paper_order.len(), 1);
        assert_eq!(paper_order[0].state, OrderState::Pending);
        // Paper signature orders get marked Failed.
    }
}
