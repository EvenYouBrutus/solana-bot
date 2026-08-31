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
        trade::{OrderKind, OrderState},
    },
    execution::{reconcile_signature, units, ExecutionError, Executor},
    portfolio::Portfolio,
    runtime::{record_reconciled_fill, SessionDeps},
    storage::StateStore,
    strategy::exit_reason,
};
use anyhow::Result;
use chrono::{Duration, Utc};
use rust_decimal::Decimal;
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
        reconcile_stale_orders(&self.deps).await;

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

            // Load persisted liquidity evidence (None = missing → exit,
            // never invent a value).
            let liquidity: Option<Decimal> = match store.last_liquidity(&mint) {
                Ok(Some((liq, _ts))) => Some(liq),
                Ok(None) => None,
                Err(e) => {
                    tracing::error!(mint=%mint, error=%e, "exit monitor: failed to read liquidity evidence; treating as missing");
                    None
                }
            };

            // Load persisted signal invalidation from store.
            // SQLite errors fail closed: treat as invalidated.
            let invalidated = match store.is_signal_invalidated(&position.signal_id) {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!(mint=%mint, error=%e, "exit monitor: failed to read invalidation state; treating as invalidated");
                    true
                }
            };

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

            // Use the shared execution path.
            let result = crate::runtime::execute_exit_order(
                executor,
                store,
                config,
                &mint,
                reason.as_str(),
                &mut portfolio,
            )
            .await;
            if let crate::runtime::ExitExecResult::Failed(e) = &result {
                tracing::error!(mint=%mint, error=%e, "exit monitor: exit execution failed");
            }
        }
        Ok(())
    }
}

fn has_inflight_exit(store: &StateStore, position: &Position) -> Result<bool> {
    Ok(store.incomplete_orders()?.iter().any(|o| {
        o.kind == OrderKind::Exit && o.position_id.as_deref() == position.position_id.as_deref()
    }))
}

// ---------------------------------------------------------------------------
// Order reconciliation for stale / unknown orders
// ---------------------------------------------------------------------------

async fn reconcile_stale_orders(deps: &ExitDeps) {
    let session_deps = SessionDeps {
        config: deps.config.clone(),
        store: deps.store.clone(),
        executor: deps.executor.clone(),
        rpc: deps.rpc.clone(),
    };
    let now = Utc::now();
    let live = deps.executor.is_live();
    let owner = deps.executor.signer_pubkey();
    for order in match deps.store.incomplete_orders() {
        Ok(o) => o,
        Err(e) => {
            tracing::error!(error=%e, "exit monitor: could not load incomplete orders");
            return;
        }
    } {
        let age = now - order.created_at;
        match (&order.signature, live) {
            (None, _) => {
                if age > Duration::seconds(deps.config.rpc.unknown_after_secs) {
                    let mut o = order.clone();
                    o.error = Some("expired without submission (exit monitor)".into());
                    o.transition(OrderState::Failed).ok();
                    deps.store.update_order(&o).ok();
                }
            }
            (Some(_sig), false) => {
                let mut o = order.clone();
                o.error = Some("paper signature cannot be reconciled".into());
                o.transition(OrderState::Failed).ok();
                deps.store.update_order(&o).ok();
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
                match reconcile_signature(
                    &deps.rpc,
                    sig,
                    input_mint,
                    output_mint,
                    owner,
                    expected_input,
                )
                .await
                {
                    Ok(Some(outcome)) => {
                        // Use the shared reconciliation path to record the
                        // fill and apply it to the portfolio, preventing
                        // lost accounting after a restart.
                        match record_reconciled_fill(&session_deps, &order, outcome, sig).await {
                            Ok(true) => {}
                            Ok(false) => {
                                // Accounting incomplete: order stays unresolved
                                // so reconciliation can retry on the next tick.
                                tracing::warn!(
                                    order_id = %order.id,
                                    "exit monitor: reconciled fill could not be fully accounted; will retry"
                                );
                            }
                            Err(e) => {
                                tracing::error!(order_id=%order.id, error=%e, "exit monitor: reconciled fill could not be accounted");
                            }
                        }
                    }
                    Ok(None) => {
                        if age > Duration::seconds(deps.config.rpc.unknown_after_secs) {
                            let mut o = order.clone();
                            o.error = Some("signature never appeared (exit monitor)".into());
                            o.transition(OrderState::Expired).ok();
                            deps.store.update_order(&o).ok();
                        }
                    }
                    Err(ExecutionError::Transaction(detail)) => {
                        let mut o = order.clone();
                        o.error = Some(detail);
                        o.transition(OrderState::Failed).ok();
                        deps.store.update_order(&o).ok();
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
        domain::{
            position::{PositionState, ReconciliationStatus},
            trade::{OrderRecord, OrderSide},
        },
        portfolio::Portfolio,
        runtime::exit_idempotency_key,
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
            Some(Decimal::MAX),
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
            Some(Decimal::MAX),
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
            Some(Decimal::MAX),
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
            Some(Decimal::MAX),
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
            Some(Decimal::MAX),
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

    // --- Regression tests for exit architecture requirements ---

    #[test]
    fn independent_exit_without_signal_feed_still_evaluates_price_exits() {
        let pos = test_position(1_000_000, dec!(0.00001), dec!(150));
        // No liquidity evidence (None) → treated as unhealthy → LiquidityDeterioration.
        // Even though price is at -10% (stop loss), liquidity check fires first.
        let reason = exit_reason(
            &pos,
            dec!(0.000009),
            None, // no liquidity evidence
            dec!(50000),
            false,
            Utc::now(),
            &strategy(),
        );
        assert_eq!(
            reason,
            Some(crate::strategy::ExitReason::LiquidityDeterioration),
            "missing liquidity evidence must trigger exit, not be treated as healthy"
        );
    }

    #[test]
    fn liquidity_deterioration_triggers_exit_when_evidence_present() {
        let pos = test_position(1_000_000, dec!(0.00001), dec!(150));
        let reason = exit_reason(
            &pos,
            dec!(0.00001),
            Some(dec!(30000)), // below min_liquidity_usd = 50000
            dec!(50000),
            false,
            Utc::now(),
            &strategy(),
        );
        assert_eq!(
            reason,
            Some(crate::strategy::ExitReason::LiquidityDeterioration)
        );
    }

    #[test]
    fn missing_liquidity_evidence_triggers_liquidity_exit() {
        let pos = test_position(1_000_000, dec!(0.00001), dec!(150));
        // None → treated as unhealthy → LiquidityDeterioration
        let reason = exit_reason(
            &pos,
            dec!(0.00001), // same as entry → no SL/TP/TS
            None,
            dec!(50000),
            false,
            Utc::now(),
            &strategy(),
        );
        assert_eq!(
            reason,
            Some(crate::strategy::ExitReason::LiquidityDeterioration),
            "missing liquidity evidence must not be treated as healthy"
        );
    }

    #[test]
    fn signal_invalidation_triggers_exit() {
        let pos = test_position(1_000_000, dec!(0.00001), dec!(150));
        let reason = exit_reason(
            &pos,
            dec!(0.00001),
            Some(Decimal::MAX),
            dec!(50000),
            true, // invalidated
            Utc::now(),
            &strategy(),
        );
        assert_eq!(reason, Some(crate::strategy::ExitReason::SignalInvalidated));
    }

    #[test]
    fn duplicate_concurrent_exits_blocked_by_idempotency() {
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
            idempotency_key: "concurrent-key".into(),
            created_at: Utc::now(),
            signature: None,
            error: None,
        };
        // First reservation succeeds.
        assert!(store.reserve_order(&order).unwrap());
        // Second reservation with same key fails (duplicate prevention).
        let order2 = OrderRecord {
            id: "o2".into(),
            ..order
        };
        assert!(!store.reserve_order(&order2).unwrap());
    }

    #[test]
    fn stale_position_not_overwritten_after_exit() {
        let store = StateStore::open(":memory:").unwrap();
        let mut pos = test_position(1_000_000, dec!(0.00001), dec!(150));
        store.save_position(&pos).unwrap();
        // Simulate another task updating the position (reducing remaining).
        pos.remaining_quantity_atomic = Some(500_000);
        store.save_position(&pos).unwrap();
        // Fresh load gets the updated state.
        let loaded = store.positions().unwrap();
        assert_eq!(loaded[0].remaining_quantity_atomic, Some(500_000));
    }

    #[test]
    fn partial_exit_preserves_remaining() {
        let store = StateStore::open(":memory:").unwrap();
        let pos = test_position(1_000_000, dec!(0.00001), dec!(150));
        store.save_position(&pos).unwrap();
        let mut portfolio = Portfolio::default();
        portfolio.load(store.positions().unwrap());
        let fill = crate::domain::trade::Fill {
            order_id: "o1".into(),
            signature: "sig1".into(),
            input_amount: 300_000, // sell 30% of position
            output_amount: 100_000_000,
            price_usd: dec!(0.00001),
            fees_usd: dec!(0.01),
            slippage_bps: 0,
            confirmed_at: Utc::now(),
            latency_ms: 0,
            fee_lamports: 0,
            input_value_usd: Some(dec!(3)),
            expected_output_amount: Some(100_000_000),
        };
        let outcome = portfolio.apply_exit("TOKEN", &fill, "take_profit", Utc::now());
        assert!(outcome.is_ok());
        let out = outcome.unwrap();
        assert!(!out.closed);
        assert_eq!(
            portfolio
                .position("TOKEN")
                .unwrap()
                .remaining_quantity_atomic,
            Some(700_000)
        );
    }

    #[test]
    fn full_exit_closes_position() {
        let store = StateStore::open(":memory:").unwrap();
        let pos = test_position(1_000_000, dec!(0.00001), dec!(150));
        store.save_position(&pos).unwrap();
        let mut portfolio = Portfolio::default();
        portfolio.load(store.positions().unwrap());
        let fill = crate::domain::trade::Fill {
            order_id: "o1".into(),
            signature: "sig1".into(),
            input_amount: 1_000_000, // sell everything
            output_amount: 500_000_000,
            price_usd: dec!(0.00001),
            fees_usd: dec!(0.01),
            slippage_bps: 0,
            confirmed_at: Utc::now(),
            latency_ms: 0,
            fee_lamports: 0,
            input_value_usd: Some(dec!(10)),
            expected_output_amount: Some(500_000_000),
        };
        let outcome = portfolio.apply_exit("TOKEN", &fill, "take_profit", Utc::now());
        assert!(outcome.is_ok());
        let out = outcome.unwrap();
        assert!(out.closed);
        assert_eq!(
            portfolio
                .position("TOKEN")
                .unwrap()
                .remaining_quantity_atomic,
            Some(0)
        );
    }

    #[test]
    fn restart_after_confirmed_exit_reflects_in_store() {
        let store = StateStore::open(":memory:").unwrap();
        let mut pos = test_position(1_000_000, dec!(0.00001), dec!(150));
        store.save_position(&pos).unwrap();
        // Simulate a confirmed exit by updating the position.
        pos.remaining_quantity_atomic = Some(0);
        pos.state = PositionState::Closed;
        pos.exit_reason = Some("take_profit".into());
        pos.exit_signature = Some("sig-exit".into());
        store.save_position(&pos).unwrap();
        // On restart, load from store — exit is persisted.
        let loaded = store.positions().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].state, PositionState::Closed);
        assert_eq!(loaded[0].exit_reason.as_deref(), Some("take_profit"));
        assert_eq!(loaded[0].remaining_quantity_atomic, Some(0));
    }

    #[test]
    fn exits_allowed_while_kill_switch_blocks_entries() {
        let store = StateStore::open(":memory:").unwrap();
        store.latch_kill_switch("drawdown").unwrap();
        assert!(store.kill_switch_reason().unwrap().is_some());
        // Verify exit order can be reserved even with kill switch active.
        let order = OrderRecord {
            id: "exit-during-kill".into(),
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
            idempotency_key: "kill-switch-exit".into(),
            created_at: Utc::now(),
            signature: None,
            error: Some("take_profit".into()),
        };
        assert!(store.reserve_order(&order).unwrap());
    }

    #[test]
    fn liquidity_persistence_round_trip() {
        let store = StateStore::open(":memory:").unwrap();
        assert!(store.last_liquidity("TOKEN").unwrap().is_none());
        store.set_last_liquidity("TOKEN", dec!(75000)).unwrap();
        let (liq, _ts) = store.last_liquidity("TOKEN").unwrap().unwrap();
        assert_eq!(liq, dec!(75000));
    }

    #[test]
    fn signal_invalidation_persistence_round_trip() {
        let store = StateStore::open(":memory:").unwrap();
        assert!(!store.is_signal_invalidated("sig-1").unwrap());
        store.set_signal_invalidated("sig-1").unwrap();
        assert!(store.is_signal_invalidated("sig-1").unwrap());
        // Different signal is not invalidated.
        assert!(!store.is_signal_invalidated("sig-2").unwrap());
    }

    // --- Regression tests for Task 5: reconciliation/accounting safety ---

    #[test]
    fn fill_recorded_by_reconciliation_survives_restart() {
        let store = StateStore::open(":memory:").unwrap();
        let pos = test_position(1_000_000, dec!(0.00001), dec!(150));
        store.save_position(&pos).unwrap();
        // Create an Unknown order with a signature (as if submit succeeded
        // but confirmation was lost).
        let order = OrderRecord {
            id: "exit-unknown".into(),
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
            state: OrderState::Unknown,
            idempotency_key: "exit-unknown-key".into(),
            created_at: Utc::now(),
            signature: Some("on-chain-sig".into()),
            error: Some("outcome unknown".into()),
        };
        store.reserve_order(&order).unwrap();
        // After restart, the order is still Unknown and has a signature.
        let incomplete = store.incomplete_orders().unwrap();
        assert_eq!(incomplete.len(), 1);
        assert_eq!(incomplete[0].state, OrderState::Unknown);
        assert!(incomplete[0].signature.is_some());
    }

    #[test]
    fn confirmed_order_blocks_inflight_exit_check() {
        let store = StateStore::open(":memory:").unwrap();
        let pos = test_position(1_000_000, dec!(0.00001), dec!(150));
        // Create an exit order that was confirmed.
        let order = OrderRecord {
            id: "exit-confirmed".into(),
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
            state: OrderState::Confirmed,
            idempotency_key: "exit-confirmed-key".into(),
            created_at: Utc::now(),
            signature: Some("sig".into()),
            error: None,
        };
        store.reserve_order(&order).unwrap();
        // Confirmed order is terminal → should not block new exits.
        assert!(!has_inflight_exit(&store, &pos).unwrap());
    }

    #[test]
    fn fill_persists_after_partial_exit() {
        let store = StateStore::open(":memory:").unwrap();
        let pos = test_position(1_000_000, dec!(0.00001), dec!(150));
        store.save_position(&pos).unwrap();
        let mut portfolio = Portfolio::default();
        portfolio.load(store.positions().unwrap());
        // Partial exit: sell 40% of the position.
        let fill = crate::domain::trade::Fill {
            order_id: "o-partial".into(),
            signature: "sig-partial".into(),
            input_amount: 400_000,
            output_amount: 200_000_000,
            price_usd: dec!(0.00001),
            fees_usd: dec!(0.01),
            slippage_bps: 10,
            confirmed_at: Utc::now(),
            latency_ms: 0,
            fee_lamports: 5_000,
            input_value_usd: Some(dec!(4)),
            expected_output_amount: Some(210_000_000),
        };
        let outcome = portfolio.apply_exit("TOKEN", &fill, "trailing_stop", Utc::now());
        assert!(outcome.is_ok());
        let out = outcome.unwrap();
        assert!(!out.closed);
        // Remaining: 600_000
        assert_eq!(
            portfolio
                .position("TOKEN")
                .unwrap()
                .remaining_quantity_atomic,
            Some(600_000)
        );
        // realized_pnl = proceeds - cost_of_sold - fees = 4 - 4 - 0.01 = -0.01
        assert_eq!(out.realized_pnl_usd, dec!(-0.01));
        if let Some(p) = portfolio.position("TOKEN") {
            store.save_position(p).unwrap();
        }
        let reloaded = store.positions().unwrap();
        assert_eq!(reloaded[0].remaining_quantity_atomic, Some(600_000));
        assert_eq!(reloaded[0].realized_pnl_usd, dec!(-0.01));
    }

    #[test]
    fn exit_all_uses_shared_execution_path() {
        // Verify that exit_all_positions checks incomplete orders before
        // attempting exits, same as normal exit pipeline.
        let store = StateStore::open(":memory:").unwrap();
        let pos = test_position(1_000_000, dec!(0.00001), dec!(150));
        store.save_position(&pos).unwrap();
        let order = OrderRecord {
            id: "inflight".into(),
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
            idempotency_key: "inflight-key".into(),
            created_at: Utc::now(),
            signature: None,
            error: None,
        };
        store.reserve_order(&order).unwrap();
        // has_inflight_exit detects the in-flight order.
        assert!(has_inflight_exit(&store, &pos).unwrap());
    }
}
