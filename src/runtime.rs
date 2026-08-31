//! Trading runtime: restart reconciliation, entry pipeline, exit pipeline,
//! and the persistent safety interlocks (kill switch, emergency stop).
//!
//! Every submission is verified on-chain before any fill is accounted, and
//! every order's lifecycle is persisted so a crash at any point can be
//! recovered without duplicate trades.

use crate::{
    config::types::Config,
    data::rpc::RpcPool,
    domain::{
        position::Position,
        trade::{Fill, OrderKind, OrderRecord, OrderSide, OrderState},
        wallet::WalletStats,
    },
    economics::{CostModel, EconomicGate, ExpectedValue},
    execution::{
        finalize_fill, reconcile_signature, units, ExecutionError, ExecutionRequest, Executor,
        ValueBasis,
    },
    exit_monitor::{ExitDeps, ExitMonitor},
    portfolio::{ExitOutcome, Portfolio},
    risk::{authorize_entry, RiskEngine},
    storage::StateStore,
    strategy::{evaluate_signal, exit_reason, StrategyDecision},
};
use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Explicit, timestamped boundary between a verified collector and this trading
/// process. A missing/incomplete record is rejected; it is never synthesized.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CandidateInput {
    pub mint: String,
    #[serde(default)]
    pub token_decimals: Option<u8>,
    #[serde(default)]
    pub base_mint_decimals: Option<u8>,
    pub input_amount: u64,
    pub position_usd: Decimal,
    pub expected_gross_return_pct: Decimal,
    pub market: crate::domain::market::MarketSnapshot,
    pub safety: crate::domain::token::TokenSafety,
    pub wallets: Vec<WalletStats>,
    pub costs: CostModel,
}

pub struct SessionDeps {
    pub config: Arc<Config>,
    pub store: Arc<StateStore>,
    pub executor: Arc<dyn Executor>,
    pub rpc: Arc<RpcPool>,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct ReconcileSummary {
    pub confirmed_orders: u32,
    pub failed_orders: u32,
    pub expired_orders: u32,
    pub unresolved_orders: u32,
    pub positions_reconciled: u32,
    pub positions_adjusted: u32,
    pub positions_closed: u32,
    pub onchain_errors: u32,
}

/// Order lifecycle state that blocks new entries until resolved.
pub fn entries_blocked_by(store: &StateStore) -> Result<bool> {
    Ok(!store.incomplete_orders()?.is_empty())
}

/// Marks stale signature-less orders as failed (they were never submitted)
/// and reconciles orders that carry a signature against the chain. Never
/// sends a new transaction; it only resolves what already happened.
pub async fn reconcile_pending_orders(deps: &SessionDeps) -> Result<ReconcileSummary> {
    let mut summary = ReconcileSummary::default();
    let now = Utc::now();
    let live = deps.executor.is_live();
    let owner = deps.executor.signer_pubkey();
    for order in deps.store.incomplete_orders()? {
        let age = now - order.created_at;
        match (&order.signature, live) {
            (None, _) => {
                if age > Duration::seconds(deps.config.rpc.unknown_after_secs) {
                    let mut o = order.clone();
                    o.error = Some("order expired without ever being submitted".into());
                    o.transition(OrderState::Failed).unwrap_or_else(|e| tracing::error!(order_id=%o.id, %e, "state machine blocked failure marking"));
                    deps.store.update_order(&o)?;
                    summary.failed_orders += 1;
                } else {
                    summary.unresolved_orders += 1;
                }
            }
            (Some(sig), false) => {
                // Paper-mode signature: nothing on-chain to reconcile.
                let mut o = order.clone();
                o.error = Some(format!(
                    "paper signature {sig} cannot be reconciled on-chain"
                ));
                o.transition(OrderState::Failed).unwrap_or_else(|e| tracing::error!(order_id=%o.id, %e, "state machine blocked failure marking"));
                deps.store.update_order(&o)?;
                summary.failed_orders += 1;
            }
            (Some(sig), true) => {
                let Some(owner) = &owner else {
                    tracing::error!(order_id=%order.id, "live order cannot be reconciled without a signer pubkey");
                    summary.unresolved_orders += 1;
                    continue;
                };
                let (Some(input_mint), Some(output_mint), Some(expected_input)) = (
                    &order.input_mint,
                    &order.output_mint,
                    order.input_amount_atomic,
                ) else {
                    tracing::error!(order_id=%order.id, "legacy order lacks route metadata; operator review required");
                    summary.unresolved_orders += 1;
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
                        match record_reconciled_fill(deps, &order, outcome, sig).await {
                            Ok(true) => summary.confirmed_orders += 1,
                            Ok(false) => summary.unresolved_orders += 1,
                            Err(e) => {
                                tracing::error!(order_id=%order.id, error=%e, "reconciled fill could not be accounted; operator review required");
                                summary.unresolved_orders += 1;
                            }
                        }
                    }
                    Ok(None) => {
                        if age > Duration::seconds(deps.config.rpc.unknown_after_secs) {
                            let mut o = order.clone();
                            o.error = Some(
                                "signature never appeared on-chain within the expiry window".into(),
                            );
                            o.transition(OrderState::Expired).unwrap_or_else(|e| tracing::error!(order_id=%o.id, %e, "state machine blocked expiry marking"));
                            deps.store.update_order(&o)?;
                            summary.expired_orders += 1;
                        } else {
                            summary.unresolved_orders += 1;
                        }
                    }
                    Err(ExecutionError::Transaction(detail)) => {
                        let mut o = order.clone();
                        o.error = Some(detail);
                        o.transition(OrderState::Failed).unwrap_or_else(|e| tracing::error!(order_id=%o.id, %e, "state machine blocked failure marking"));
                        deps.store.update_order(&o)?;
                        summary.failed_orders += 1;
                    }
                    Err(e) => {
                        tracing::warn!(order_id=%order.id, error=%e, "reconciliation attempt did not resolve order");
                        summary.unresolved_orders += 1;
                    }
                }
            }
        }
    }
    Ok(summary)
}

/// Applies a verified on-chain outcome to an order created in a previous
/// process. Returns Ok(false) when accounting cannot be completed safely.
async fn record_reconciled_fill(
    deps: &SessionDeps,
    order: &OrderRecord,
    outcome: crate::execution::reconcile::ChainSwapOutcome,
    signature: &str,
) -> Result<bool> {
    let position = order.position_id.as_ref().and_then(|pid| {
        deps.store
            .positions()
            .ok()?
            .into_iter()
            .find(|p| p.position_id.as_deref() == Some(pid))
    });
    let (Some(position), Some(input_value_usd)) = (&position, order.input_value_usd) else {
        let mut o = order.clone();
        o.error = Some(
            "reconciled on-chain but position/value basis is missing; no accounting applied".into(),
        );
        o.transition(OrderState::Reconciled)
            .map_err(|e| anyhow::anyhow!(e))?;
        deps.store.update_order(&o)?;
        return Ok(false);
    };
    let (Some(token_decimals), Some(base_decimals)) =
        (position.token_decimals, position.base_mint_decimals)
    else {
        return Ok(false);
    };
    let basis = match order.kind {
        OrderKind::Entry => ValueBasis::InputValueUsd(input_value_usd),
        _ => match position.base_entry_price_usd {
            Some(price) => ValueBasis::OutputUnitPriceUsd(price),
            None => return Ok(false),
        },
    };
    let (input_decimals, output_decimals) = match order.kind {
        OrderKind::Entry => (base_decimals, token_decimals),
        _ => (token_decimals, base_decimals),
    };
    let Ok((value_usd, price_usd)) = basis.price_fill(
        outcome.input_amount,
        input_decimals,
        outcome.output_amount,
        output_decimals,
    ) else {
        return Ok(false);
    };
    let mut fill = Fill {
        order_id: order.id.clone(),
        signature: signature.to_string(),
        input_amount: outcome.input_amount,
        output_amount: outcome.output_amount,
        price_usd,
        fees_usd: crate::execution::executor::fee_usd(
            outcome.fee_lamports,
            deps.config.economics.sol_price_usd,
        ),
        slippage_bps: 0,
        confirmed_at: Utc::now(),
        latency_ms: 0,
        fee_lamports: outcome.fee_lamports,
        input_value_usd: Some(value_usd),
        expected_output_amount: None,
    };
    let mut o = order.clone();
    o.signature = Some(signature.to_string());
    o.transition(OrderState::Confirmed)
        .map_err(|e| anyhow::anyhow!(e))?;
    deps.store.update_order(&o)?;
    deps.store.save_fill(&fill)?;
    apply_confirmed_fill_to_portfolio(
        deps,
        order,
        position,
        &mut fill,
        token_decimals,
        base_decimals,
    )?;
    tracing::info!(order_id=%order.id, kind=?order.kind, mint=%order.mint, %signature,
        input=outcome.input_amount, output=outcome.output_amount, fee_lamports=outcome.fee_lamports,
        "order reconciled from chain after restart");
    Ok(true)
}

/// Shared accounting for a confirmed fill, whether executed live this session
/// or reconciled after a restart.
fn apply_confirmed_fill_to_portfolio(
    deps: &SessionDeps,
    order: &OrderRecord,
    position: &Position,
    fill: &mut Fill,
    token_decimals: u8,
    base_decimals: u8,
) -> Result<()> {
    let mut portfolio = load_portfolio(&deps.store)?;
    let result = match order.kind {
        OrderKind::Entry => {
            let cost_model = position
                .entry_cost_model
                .clone()
                .unwrap_or_else(|| CostModel {
                    observed_at: Utc::now(),
                    source: "reconciled".into(),
                    is_live_snapshot: false,
                    input: crate::economics::BreakEvenInputs {
                        position_size_usd: Decimal::ONE,
                        avg_priority_fee_usd: Decimal::ZERO,
                        avg_swap_fee_bps: Decimal::ZERO,
                        avg_slippage_bps: Decimal::ZERO,
                        avg_price_impact_bps: Decimal::ZERO,
                        failed_tx_rate: Decimal::ZERO,
                        avg_failed_tx_cost_usd: Decimal::ZERO,
                        assumed_win_loss_ratio: Decimal::ONE,
                        assumed_avg_loss_pct: Decimal::ONE,
                    },
                });
            portfolio.apply_entry(
                order.mint.clone(),
                position.base_mint.clone().unwrap_or_default(),
                token_decimals,
                base_decimals,
                order.position_id.clone().unwrap_or_default(),
                fill,
                fill.price_usd,
                order.signal_id.clone(),
                cost_model,
            )
        }
        _ => portfolio
            .apply_exit(
                &order.mint,
                fill,
                order.error.as_deref().unwrap_or("reconciled"),
                Utc::now(),
            )
            .map(|_| ()),
    };
    match result {
        Ok(()) => {
            if let Some(p) = portfolio.position(&order.mint) {
                deps.store.save_position(p)?;
            }
            Ok(())
        }
        Err(e) => Err(anyhow::anyhow!(e)),
    }
}

fn load_portfolio(store: &StateStore) -> Result<Portfolio> {
    let mut portfolio = Portfolio::default();
    portfolio.load(store.positions()?);
    Ok(portfolio)
}

/// Compares persisted open positions with actual wallet token balances.
/// Only runs with a live signer; paper positions keep their recorded state.
pub async fn reconcile_positions_onchain(deps: &SessionDeps) -> Result<ReconcileSummary> {
    let mut summary = ReconcileSummary::default();
    let Some(owner) = deps.executor.signer_pubkey() else {
        return Ok(summary);
    };
    let balances = match deps.rpc.token_balances(&owner).await {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error=%e, "could not read on-chain token balances; entries stay blocked until reconciliation succeeds");
            summary.onchain_errors += 1;
            return Ok(summary);
        }
    };
    let by_mint: HashMap<&str, &crate::data::rpc::TokenBalance> =
        balances.iter().map(|b| (b.mint.as_str(), b)).collect();
    let mut portfolio = load_portfolio(&deps.store)?;
    for position in portfolio
        .open_positions()
        .into_iter()
        .map(|p| p.mint.clone())
        .collect::<Vec<_>>()
    {
        let now = Utc::now();
        match by_mint.get(position.as_str()) {
            None => {
                if portfolio.reconcile_with_chain(&position, 0, 0, now).is_ok() {
                    summary.positions_closed += 1;
                    if let Some(p) = portfolio.position(&position) {
                        deps.store.save_position(p)?;
                    }
                }
            }
            Some(balance) => match portfolio.reconcile_with_chain(
                &position,
                balance.amount,
                balance.decimals,
                now,
            ) {
                Ok(true) => {
                    if portfolio
                        .position(&position)
                        .map(|p| p.reconciliation_status)
                        == Some(crate::domain::position::ReconciliationStatus::AdjustedOnChain)
                    {
                        summary.positions_adjusted += 1;
                    } else {
                        summary.positions_reconciled += 1;
                    }
                    if let Some(p) = portfolio.position(&position) {
                        deps.store.save_position(p)?;
                    }
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::error!(error=%e, mint=%position, "position reconciliation failed; entries blocked");
                    summary.onchain_errors += 1;
                }
            },
        }
    }
    Ok(summary)
}

/// Marks every open position to the price implied by a fresh sell quote.
pub(crate) fn mark_price_from_quote(
    quote_output_atomic: u64,
    base_decimals: u8,
    base_price_usd: Decimal,
    token_remaining_atomic: u64,
    token_decimals: u8,
) -> Option<Decimal> {
    if token_remaining_atomic == 0 || quote_output_atomic == 0 {
        return None;
    }
    let base_units = units(quote_output_atomic, base_decimals).ok()?;
    let token_units = units(token_remaining_atomic, token_decimals).ok()?;
    if token_units.is_zero() {
        return None;
    }
    Some(base_units * base_price_usd / token_units)
}

fn exit_idempotency_key(position_id: &str, remaining: u64, reason: &str, attempt: u32) -> String {
    let mut hash = Sha256::new();
    hash.update(position_id.as_bytes());
    hash.update(remaining.to_le_bytes());
    hash.update(reason.as_bytes());
    hash.update(attempt.to_le_bytes());
    format!("{:x}", hash.finalize())
}

fn equity_usd(starting: Decimal, positions: &[&Position]) -> Decimal {
    let mut equity = starting;
    for p in positions {
        equity += p.realized_pnl_usd + p.unrealized_pnl_usd;
    }
    equity
}

struct SessionState {
    portfolio: Portfolio,
    risk: RiskEngine,
    candidates: HashMap<String, CandidateInput>,
    seen_signals: HashSet<String>,
    day_key: String,
    emergency_active: bool,
}

/// Runs the trading session until `shutdown` resolves. Entries, exits,
/// reconciliation, and interlocks all execute here; the executor cannot
/// bypass the risk layer because it is only ever invoked from this loop.
pub async fn run_session(
    deps: SessionDeps,
    shutdown: tokio::sync::oneshot::Receiver<()>,
) -> Result<()> {
    let config = &deps.config;
    let store = &deps.store;
    let mut risk = RiskEngine::new(config.risk.clone(), config.risk.starting_capital_usd);
    let portfolio = load_portfolio(store)?;
    risk.state.open_positions = portfolio.open_positions().len();
    let mut state = SessionState {
        portfolio,
        risk,
        candidates: HashMap::new(),
        seen_signals: HashSet::new(),
        day_key: String::new(),
        emergency_active: false,
    };

    // Restore or initialise the daily-loss window.
    let today = Utc::now().date_naive().to_string();
    state.day_key = today;
    let stored_day: Option<String> = store.get("runtime:day")?;
    let stored_start: Option<Decimal> = store.get("runtime:day_start_equity")?;
    let stored_trades: Option<u32> = store.get("runtime:trades_today")?;
    if stored_day.as_deref() == Some(state.day_key.as_str()) {
        if let Some(start) = stored_start {
            state.risk.state.day_start_equity_usd = start;
        }
        state.risk.state.trades_today = stored_trades.unwrap_or(0);
    }
    if let Some(reason) = store.kill_switch_reason()? {
        state.risk.kill_switch.force_trip();
        tracing::warn!(%reason, "persisted kill switch is latched; entries disabled for this session");
    }
    if let Some(reason) = store.emergency_stop()? {
        state.emergency_active = true;
        tracing::warn!(%reason, "persisted emergency stop is active; entries disabled, manual exits remain available");
    }

    let startup = if deps.executor.is_live() {
        run_reconciliation(&deps).await?
    } else {
        reconcile_pending_orders(&deps).await?
    };
    if startup.unresolved_orders > 0 || startup.onchain_errors > 0 {
        tracing::error!(
            ?startup,
            "startup reconciliation incomplete; new entries stay blocked until resolved"
        );
    }

    // Spawn the independent exit monitor.  It runs on its own cadence and
    // loads positions from SQLite, so it is completely decoupled from signal
    // ingestion and the main session tick.
    let (exit_shutdown_tx, exit_shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let exit_deps = ExitDeps {
        config: config.clone(),
        store: store.clone(),
        executor: deps.executor.clone(),
        rpc: deps.rpc.clone(),
    };
    let exit_interval = config.runtime.poll_interval_secs;
    let exit_monitor = ExitMonitor::new(exit_deps, exit_interval);
    let exit_handle = tokio::spawn(async move {
        if let Err(e) = exit_monitor.run(exit_shutdown_rx).await {
            tracing::error!(error = %e, "exit monitor terminated with error");
        }
    });

    let mut shutdown = shutdown;
    let mut last_position_reconcile = std::time::Instant::now();
    loop {
        tokio::select! {
            _ = &mut shutdown => { tracing::info!("shutdown signal received; draining session"); break; }
            _ = tokio::time::sleep(std::time::Duration::from_secs(config.runtime.poll_interval_secs)) => {}
        }
        if let Err(e) = tick(&deps, &mut state, &mut last_position_reconcile).await {
            tracing::error!(error = %e, "session tick failed; continuing with interlocks engaged");
        }
        persist_session_state(store, &state)?;
    }

    // Shut down the exit monitor.
    let _ = exit_shutdown_tx.send(());
    let _ = exit_handle.await;

    // Final reconciliation pass so shutdown state is auditable.
    if deps.executor.is_live() {
        match reconcile_pending_orders(&deps).await {
            Ok(s) => tracing::info!(?s, "final order reconciliation complete"),
            Err(e) => tracing::error!(error = %e, "final reconciliation failed"),
        }
    }
    Ok(())
}

pub async fn run_reconciliation(deps: &SessionDeps) -> Result<ReconcileSummary> {
    let mut summary = reconcile_pending_orders(deps).await?;
    let pos = reconcile_positions_onchain(deps).await?;
    summary.positions_reconciled += pos.positions_reconciled;
    summary.positions_adjusted += pos.positions_adjusted;
    summary.positions_closed += pos.positions_closed;
    summary.onchain_errors += pos.onchain_errors;
    Ok(summary)
}

async fn tick(
    deps: &SessionDeps,
    state: &mut SessionState,
    last_position_reconcile: &mut std::time::Instant,
) -> Result<()> {
    let config = &deps.config;
    let store = &deps.store;
    let now = Utc::now();

    // Daily-loss window rollover.
    let today = now.date_naive().to_string();
    if today != state.day_key {
        state.day_key = today;
        state.risk.state.day_start_equity_usd = state.risk.state.equity_usd;
        state.risk.state.trades_today = 0;
        tracing::info!("daily risk window reset");
    }

    // Recurring on-chain reconciliation (live only).
    if deps.executor.is_live()
        && config.runtime.reconcile_interval_secs > 0
        && last_position_reconcile.elapsed()
            >= std::time::Duration::from_secs(config.runtime.reconcile_interval_secs)
    {
        match reconcile_positions_onchain(deps).await {
            Ok(s) if s.onchain_errors == 0 => {
                tracing::info!(?s, "periodic position reconciliation complete")
            }
            Ok(s) => tracing::error!(
                ?s,
                "periodic reconciliation had errors; entries stay blocked"
            ),
            Err(e) => tracing::error!(error = %e, "periodic reconciliation failed"),
        }
        *last_position_reconcile = std::time::Instant::now();
    }

    // Resolve any orders stuck in Unknown before anything else.
    if let Ok(s) = reconcile_pending_orders(deps).await {
        if s.unresolved_orders > 0 {
            tracing::warn!(
                unresolved = s.unresolved_orders,
                "orders still unresolved; entries blocked"
            );
        }
    }

    // Emergency stop is re-read every tick so operator changes apply live.
    match store.emergency_stop() {
        Ok(Some(reason)) => {
            if !state.emergency_active {
                tracing::error!(%reason, "EMERGENCY STOP engaged: no new trades");
            }
            state.emergency_active = true;
        }
        Ok(None) => state.emergency_active = false,
        Err(e) => {
            tracing::error!(error = %e, "cannot read emergency stop state; failing closed");
            state.emergency_active = true;
        }
    }

    // Persisted kill switch may be latched by operators between ticks.
    if let Some(reason) = store.kill_switch_reason()? {
        state.risk.kill_switch.force_trip();
        let _ = reason;
    }

    // 1. Ingest candidates from the verified feed.
    if let Some(path) = config
        .runtime
        .signal_feed_path
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        match std::fs::read_to_string(path) {
            Ok(data) => {
                for line in data.lines().filter(|l| !l.trim().is_empty()) {
                    match serde_json::from_str::<CandidateInput>(line) {
                        Ok(c) => {
                            let fresh = now;
                            if c.market.observed_at > fresh || c.safety.observed_at > fresh {
                                tracing::warn!(mint=%c.mint, "rejected future-dated candidate");
                                continue;
                            }
                            state.candidates.insert(c.mint.clone(), c);
                        }
                        Err(e) => tracing::warn!(error = %e, "skipping malformed candidate line"),
                    }
                }
            }
            Err(e) => {
                tracing::error!(error = %e, path, "cannot read signal feed; no entries this tick")
            }
        }
    }

    // 2. Equity marking and risk state.
    let open: Vec<&Position> = state.portfolio.open_positions();
    let equity = equity_usd(config.risk.starting_capital_usd, &open);
    state.risk.observe_equity(equity);
    if state.risk.kill_switch.is_tripped() && store.kill_switch_reason()?.is_none() {
        store.latch_kill_switch("drawdown limit reached")?;
        tracing::error!("KILL SWITCH latched: drawdown limit reached; entries disabled");
    }
    state.risk.state.open_positions = open.len();

    // 3. Exits first: risk-reducing, allowed under interlocks.
    process_exits(deps, state).await?;

    // 4. Entries: blocked by emergency stop, kill switch, or unresolved state.
    if state.emergency_active {
        tracing::debug!("emergency stop active; skipping entries");
    } else if state.risk.kill_switch.is_tripped() {
        tracing::debug!("kill switch latched; skipping entries");
    } else if entries_blocked_by(store)? {
        tracing::warn!("unreconciled orders exist; refusing new entries");
    } else if config
        .runtime
        .signal_feed_path
        .as_deref()
        .unwrap_or_default()
        .is_empty()
    {
        tracing::debug!("no signal feed configured; no entries");
    } else {
        process_entries(deps, state).await?;
    }
    Ok(())
}

async fn process_exits(deps: &SessionDeps, state: &mut SessionState) -> Result<()> {
    let config = &deps.config;
    let open: Vec<String> = state
        .portfolio
        .open_positions()
        .iter()
        .map(|p| p.mint.clone())
        .collect();
    for mint in open {
        let Some(position) = state.portfolio.position(&mint).cloned() else {
            continue;
        };
        let Some(remaining) = position.trusted_remaining() else {
            tracing::error!(mint=%mint, position_id=?position.position_id, "position quantity is not trusted; refusing to sell an assumed balance");
            continue;
        };
        // Incomplete order for this position: its outcome must resolve first.
        if deps.store.incomplete_orders()?.iter().any(|o| {
            o.position_id.as_deref() == position.position_id.as_deref() && o.kind == OrderKind::Exit
        }) {
            tracing::debug!(mint=%mint, "exit order already in flight; waiting for reconciliation");
            continue;
        }
        let Some(base_price) = position.base_entry_price_usd else {
            tracing::error!(mint=%mint, "position lacks base price basis; cannot mark");
            continue;
        };
        let (Some(base_decimals), Some(token_decimals)) =
            (position.base_mint_decimals, position.token_decimals)
        else {
            continue;
        };
        let candidate = state.candidates.get(&mint);
        let liquidity = candidate
            .map(|c| c.market.liquidity_usd)
            .unwrap_or(Decimal::MAX);
        // Exits are independent of the candidate feed; they must be evaluated
        // on every tick regardless of whether new entry signals are present.
        let invalidated = false;
        let Some(mark) = derive_mark(
            deps,
            state,
            &mint,
            remaining,
            base_decimals,
            token_decimals,
            base_price,
        )
        .await
        else {
            continue;
        };
        state
            .portfolio
            .mark_to_market(&mint, mark)
            .map_err(|e| anyhow::anyhow!(e))?;
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
        attempt_exit(deps, state, &mint, remaining, reason.as_str(), false).await?;
    }
    Ok(())
}

/// Fresh sell quote establishes the current mark price; no synthetic prices.
async fn derive_mark(
    deps: &SessionDeps,
    state: &SessionState,
    mint: &str,
    remaining: u64,
    base_decimals: u8,
    token_decimals: u8,
    base_price: Decimal,
) -> Option<Decimal> {
    let base_mint = state.portfolio.position(mint)?.base_mint.clone()?;
    match deps
        .executor
        .quote(
            mint,
            &base_mint,
            remaining,
            deps.config.execution.slippage_bps,
        )
        .await
    {
        Ok(q) => mark_price_from_quote(
            q.output_amount,
            base_decimals,
            base_price,
            remaining,
            token_decimals,
        ),
        Err(e) => {
            tracing::warn!(mint=%mint, error=%e, "mark quote unavailable; exit evaluation skipped this tick");
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn attempt_exit(
    deps: &SessionDeps,
    state: &mut SessionState,
    mint: &str,
    remaining: u64,
    reason: &str,
    manual: bool,
) -> Result<()> {
    let config = &deps.config;
    let store = &deps.store;
    let now = Utc::now();
    if let Err(e) = state.risk.authorize_exit(remaining) {
        tracing::warn!(mint=%mint, error=%e, "exit refused by risk engine");
        return Ok(());
    }
    let Some(position) = state.portfolio.position(mint).cloned() else {
        return Ok(());
    };
    let Some(token_decimals) = position.token_decimals else {
        return Ok(());
    };
    let Some(base_decimals) = position.base_mint_decimals else {
        return Ok(());
    };
    let Some(base_mint) = position.base_mint.clone() else {
        return Ok(());
    };
    let Some(base_price) = position.base_entry_price_usd else {
        return Ok(());
    };
    let attempt = store
        .orders()?
        .iter()
        .filter(|o| {
            o.kind == OrderKind::Exit && o.position_id.as_deref() == position.position_id.as_deref()
        })
        .count() as u32;
    let key = exit_idempotency_key(
        position.position_id.as_deref().unwrap_or(mint),
        remaining,
        reason,
        attempt,
    );
    let mark_value = match units(remaining, token_decimals) {
        Ok(q) => q * base_price,
        Err(_) => return Ok(()),
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
    if !store.reserve_order(&order)? {
        tracing::warn!(mint=%mint, "duplicate exit order blocked by idempotency");
        return Ok(());
    }
    let quote = match deps
        .executor
        .quote(mint, &base_mint, remaining, config.execution.slippage_bps)
        .await
    {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(mint=%mint, error=%e, "exit quote unavailable; no transaction sent");
            let mut o = order.clone();
            o.error = Some(format!("quote failed: {e}"));
            o.transition(OrderState::Failed).ok();
            store.update_order(&o)?;
            return Ok(());
        }
    };
    if quote.price_impact_bps > config.execution.max_price_impact_bps {
        tracing::error!(mint=%mint, impact_bps=quote.price_impact_bps, cap=config.execution.max_price_impact_bps, "exit quote exceeds price-impact limit; not selling into this liquidity");
        let mut o = order.clone();
        o.error = Some("price impact limit".into());
        o.transition(OrderState::Failed).ok();
        store.update_order(&o)?;
        return Ok(());
    }
    let min_output = quote
        .output_amount
        .saturating_mul(10_000 - config.execution.slippage_bps as u64)
        / 10_000;
    let mut placed = order.clone();
    placed.transition(OrderState::Submitted).ok();
    store.update_order(&placed)?;
    tracing::info!(order_id=%order.id, mint=%mint, reason=%reason, qty_atomic=remaining, expected_out=quote.output_amount, impact_bps=quote.price_impact_bps, "exit order submitted");
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
    match deps.executor.execute(request).await {
        Ok(mut fill) => {
            finalize_fill(&mut fill, config.economics.sol_price_usd);
            placed.signature = Some(fill.signature.clone());
            placed.transition(OrderState::Confirmed).ok();
            store.update_order(&placed)?;
            store.save_fill(&fill)?;
            match state.portfolio.apply_exit(mint, &fill, reason, Utc::now()) {
                Ok(ExitOutcome {
                    realized_pnl_usd,
                    closed,
                }) => {
                    if let Some(p) = state.portfolio.position(mint) {
                        store.save_position(p)?;
                    }
                    state.risk.record_execution_success();
                    if realized_pnl_usd < Decimal::ZERO {
                        state.risk.apply_loss_cooldown(Utc::now());
                    }
                    if fill.output_amount < min_output {
                        tracing::error!(order_id=%order.id, actual=fill.output_amount, min_output, "exit fill fell below the minimum accepted output");
                        state.risk.record_execution_failure(Utc::now());
                    }
                    tracing::info!(order_id=%order.id, mint=%mint, %reason, manual, signature=%fill.signature, sold=fill.input_amount, received=fill.output_amount, fees_usd=%fill.fees_usd, realized_pnl_usd=%realized_pnl_usd, closed, "exit confirmed and reconciled");
                }
                Err(e) => {
                    tracing::error!(order_id=%order.id, error=%e, "confirmed exit could not be applied to the position; operator review required")
                }
            }
        }
        Err(ExecutionError::Unknown { signature, detail }) => {
            if let Some(sig) = &signature {
                placed.signature = Some(sig.clone());
            }
            placed.transition(OrderState::Unknown).ok();
            placed.error = Some(detail.clone());
            store.update_order(&placed)?;
            tracing::error!(order_id=%order.id, ?signature, %detail, "exit outcome unknown; reconciliation required before any retry");
        }
        Err(e) => {
            placed.transition(OrderState::Failed).ok();
            placed.error = Some(e.to_string());
            store.update_order(&placed)?;
            let tripped = state.risk.record_execution_failure(Utc::now());
            if tripped {
                store.latch_kill_switch("consecutive execution failures")?;
            }
            tracing::error!(order_id=%order.id, mint=%mint, error=%e, "exit failed on-chain or was refused; order marked failed");
        }
    }
    Ok(())
}

async fn process_entries(deps: &SessionDeps, state: &mut SessionState) -> Result<()> {
    let config = &deps.config;
    let store = &deps.store;
    let now = Utc::now();
    let mints: Vec<String> = state.candidates.keys().cloned().collect();
    for mint in mints {
        let Some(c) = state.candidates.get(&mint).cloned() else {
            continue;
        };
        let (Some(token_decimals), Some(base_mint_decimals)) =
            (c.token_decimals, c.base_mint_decimals)
        else {
            tracing::warn!(mint=%mint, "candidate is missing canonical mint decimals");
            continue;
        };
        if c.costs.input.position_size_usd != c.position_usd {
            tracing::warn!(mint=%mint, "candidate position and economic model disagree");
            continue;
        }
        let gate = EconomicGate {
            round_trip_cost_threshold_pct: config.economics.round_trip_cost_threshold_pct,
        };
        if authorize_entry(&state.risk.kill_switch, &gate, &c.costs).is_err() {
            tracing::info!(mint=%mint, "economic cost gate rejected candidate");
            continue;
        }
        let expected = match ExpectedValue::estimate(
            c.expected_gross_return_pct,
            &c.costs,
            Decimal::ZERO,
            Decimal::ZERO,
            config.economics.uncertainty_haircut_pct,
        ) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(mint=%mint, error=%e, "expected-value estimation failed");
                continue;
            }
        };
        let wallets: Vec<&WalletStats> = c
            .wallets
            .iter()
            .filter(|w| w.updated_at <= c.market.observed_at)
            .collect();
        let signal = match evaluate_signal(config, &mint, &wallets, &c.market, &c.safety, &expected)
        {
            StrategyDecision::Accepted(s) => s,
            StrategyDecision::Rejected(reason) => {
                tracing::info!(mint=%mint, %reason, "strategy rejected candidate");
                continue;
            }
        };
        if state.seen_signals.contains(&signal.id) {
            continue;
        }
        if c.position_usd <= Decimal::ZERO || c.position_usd > config.risk.max_live_capital_usd {
            tracing::warn!(mint=%mint, "position exceeds configured live-capital cap or is invalid");
            continue;
        }
        state.risk.state.open_positions = state.portfolio.open_positions().len();
        // Fresh quote before every entry: price impact and viability are
        // evaluated on live data, never on the feed's assertion.
        let quote = match deps
            .executor
            .quote(
                &config.strategy.base_mint,
                &mint,
                c.input_amount,
                config.execution.slippage_bps,
            )
            .await
        {
            Ok(q) => q,
            Err(e) => {
                tracing::warn!(mint=%mint, error=%e, "entry quote unavailable");
                continue;
            }
        };
        if let Err(e) = state.risk.authorize(
            c.position_usd,
            c.market.liquidity_usd,
            config.execution.slippage_bps as u32,
            quote.price_impact_bps,
            now,
        ) {
            tracing::info!(mint=%mint, reason=%e, "risk engine rejected entry");
            continue;
        }
        let mut hash = Sha256::new();
        hash.update(signal.id.as_bytes());
        hash.update(mint.as_bytes());
        hash.update(c.input_amount.to_le_bytes());
        let key = format!("{:x}", hash.finalize());
        let position_id = uuid::Uuid::new_v4().to_string();
        let order = OrderRecord {
            id: uuid::Uuid::new_v4().to_string(),
            signal_id: signal.id.clone(),
            mint: mint.clone(),
            kind: OrderKind::Entry,
            position_id: Some(position_id.clone()),
            side: OrderSide::Buy,
            input_mint: Some(config.strategy.base_mint.clone()),
            output_mint: Some(mint.clone()),
            input_amount_atomic: Some(c.input_amount),
            input_value_usd: Some(c.position_usd),
            output_mint_decimals: Some(token_decimals),
            state: OrderState::Pending,
            idempotency_key: key,
            created_at: now,
            signature: None,
            error: None,
        };
        if !store.reserve_order(&order)? {
            tracing::debug!(mint=%mint, "duplicate entry order blocked by idempotency");
            state.seen_signals.insert(signal.id);
            continue;
        }
        let min_output = quote
            .output_amount
            .saturating_mul(10_000 - config.execution.slippage_bps as u64)
            / 10_000;
        let mut placed = order.clone();
        placed.transition(OrderState::Submitted).ok();
        store.update_order(&placed)?;
        tracing::info!(order_id=%order.id, mint=%mint, position_usd=%c.position_usd, qty_in=c.input_amount, expected_out=quote.output_amount, impact_bps=quote.price_impact_bps, "entry order submitted");
        let request = ExecutionRequest {
            order_id: order.id.clone(),
            quote,
            max_slippage_bps: config.risk.max_slippage_bps,
            max_price_impact_bps: config.execution.max_price_impact_bps,
            min_output_amount: min_output,
            input_decimals: base_mint_decimals,
            output_decimals: token_decimals,
            value_basis: ValueBasis::InputValueUsd(c.position_usd),
        };
        match deps.executor.execute(request).await {
            Ok(mut fill) => {
                finalize_fill(&mut fill, config.economics.sol_price_usd);
                placed.signature = Some(fill.signature.clone());
                placed.transition(OrderState::Confirmed).ok();
                store.update_order(&placed)?;
                store.save_fill(&fill)?;
                let entry_price = fill.price_usd;
                match state.portfolio.apply_entry(
                    mint.clone(),
                    config.strategy.base_mint.clone(),
                    token_decimals,
                    base_mint_decimals,
                    position_id.clone(),
                    &fill,
                    entry_price,
                    signal.id.clone(),
                    c.costs.clone(),
                ) {
                    Ok(()) => {
                        if let Some(p) = state.portfolio.position(&mint) {
                            store.save_position(p)?;
                            tracing::info!(order_id=%order.id, mint=%mint, signature=%fill.signature, position_id=%position_id, qty_atomic=fill.output_amount, price_usd=%fill.price_usd, fees_usd=%fill.fees_usd, fee_lamports=fill.fee_lamports, "confirmed entry persisted");
                        }
                        state.risk.register_trade();
                        state.risk.record_execution_success();
                        if fill.output_amount < min_output {
                            tracing::error!(order_id=%order.id, actual=fill.output_amount, min_output, "entry fill fell below the minimum accepted output");
                            let tripped = state.risk.record_execution_failure(Utc::now());
                            if tripped {
                                store.latch_kill_switch("consecutive execution failures")?;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!(order_id=%order.id, error=%e, "confirmed entry could not be applied to the portfolio; operator review required")
                    }
                }
            }
            Err(ExecutionError::Unknown { signature, detail }) => {
                if let Some(sig) = &signature {
                    placed.signature = Some(sig.clone());
                }
                placed.transition(OrderState::Unknown).ok();
                placed.error = Some(detail.clone());
                store.update_order(&placed)?;
                tracing::error!(order_id=%order.id, ?signature, %detail, "entry outcome unknown; reconciliation required before any retry");
            }
            Err(e) => {
                placed.transition(OrderState::Failed).ok();
                placed.error = Some(e.to_string());
                store.update_order(&placed)?;
                let tripped = state.risk.record_execution_failure(Utc::now());
                if tripped {
                    store.latch_kill_switch("consecutive execution failures")?;
                }
                tracing::error!(order_id=%order.id, mint=%mint, error=%e, "entry failed or was refused; order marked failed");
            }
        }
        state.seen_signals.insert(signal.id);
    }
    Ok(())
}

fn persist_session_state(store: &StateStore, state: &SessionState) -> Result<()> {
    store.put("runtime:day", &state.day_key)?;
    store.put(
        "runtime:day_start_equity",
        &state.risk.state.day_start_equity_usd,
    )?;
    store.put("runtime:trades_today", &state.risk.state.trades_today)?;
    Ok(())
}

/// Manual operator exit of every open position with a trusted quantity.
/// Permitted while the emergency stop is active; still uses the full
/// execution pipeline and reconciliation.
pub async fn exit_all_positions(deps: &SessionDeps) -> Result<usize> {
    let portfolio = load_portfolio(&deps.store)?;
    let mut exited = 0;
    let open: Vec<String> = portfolio
        .open_positions()
        .iter()
        .map(|p| p.mint.clone())
        .collect();
    let mut state = SessionState {
        portfolio,
        risk: RiskEngine::new(
            deps.config.risk.clone(),
            deps.config.risk.starting_capital_usd,
        ),
        candidates: HashMap::new(),
        seen_signals: HashSet::new(),
        day_key: Utc::now().date_naive().to_string(),
        emergency_active: true,
    };
    for mint in open {
        let Some(remaining) = state
            .portfolio
            .position(&mint)
            .and_then(|p| p.trusted_remaining())
        else {
            tracing::error!(mint=%mint, "manual exit skipped: quantity not trusted; run reconciliation first");
            continue;
        };
        if deps.store.incomplete_orders()?.iter().any(|o| {
            o.position_id
                == state
                    .portfolio
                    .position(&mint)
                    .and_then(|p| p.position_id.clone())
                && o.kind == OrderKind::Exit
        }) {
            tracing::warn!(mint=%mint, "manual exit skipped: exit order already in flight");
            continue;
        }
        attempt_exit(deps, &mut state, &mint, remaining, "manual_exit", true).await?;
        exited += 1;
    }
    Ok(exited)
}

/// Recovers the daily-loss window after a restart (used by session startup).
pub fn day_rollover_check(now: DateTime<Utc>, stored_day: &str) -> bool {
    stored_day != now.date_naive().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::position::{PositionState, ReconciliationStatus};
    use crate::execution::executor::Quote;
    use crate::strategy::ExitReason;
    use chrono::Utc;
    use rust_decimal_macros::dec;
    use std::sync::Arc;
    use std::time::Duration;

    fn strategy_config() -> crate::config::types::StrategyConfig {
        crate::config::types::StrategyConfig {
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

    fn test_config() -> Arc<crate::config::types::Config> {
        use crate::config::types::*;
        use rust_decimal_macros::dec;
        Arc::new(Config {
            mode: Mode::Paper,
            rpc: RpcConfig {
                http_endpoints: vec!["https://api.test".into()],
                websocket_endpoints: vec![],
                max_data_age_secs: 15,
                request_timeout_secs: 8,
                max_attempts: 2,
                unknown_after_secs: 180,
            },
            strategy: strategy_config(),
            economics: EconomicsConfig {
                round_trip_cost_threshold_pct: dec!(3),
                min_expected_net_return_pct: dec!(2),
                max_quote_age_secs: 3,
                uncertainty_haircut_pct: dec!(1),
                sol_price_usd: None,
            },
            risk: RiskConfig {
                starting_capital_usd: dec!(100),
                max_live_capital_usd: dec!(25),
                max_concurrent_positions: 1,
                max_position_percent_of_equity: dec!(5),
                max_position_percent_of_liquidity: dec!(0.10),
                max_risk_per_trade_percent: dec!(0.5),
                max_daily_loss_percent: dec!(2),
                max_total_drawdown_before_kill_switch_pct: dec!(5),
                cooldown_after_loss_minutes: 30,
                max_slippage_bps: 100,
                min_liquidity_usd: dec!(50000),
                max_trades_per_day: 3,
                max_consecutive_failures: 3,
            },
            execution: ExecutionConfig {
                provider: ExecutionProvider::Jupiter,
                jupiter_api_url: "https://api.jup.ag".into(),
                slippage_bps: 75,
                priority_fee_lamports: 10_000,
                max_fee_lamports: 500_000,
                max_price_impact_bps: 300,
                confirm_timeout_secs: 90,
                confirm_poll_ms: 500,
                live_signer_env: None,
                jupiter_api_key_env: None,
                allowed_program_ids: vec![],
            },
            storage: StorageConfig {
                sqlite_path: ":memory:".into(),
            },
            runtime: RuntimeConfig::default(),
            observability: ObservabilityConfig::default(),
        })
    }

    fn position(remaining: u64, base_price: Decimal) -> Position {
        Position {
            mint: "T".into(),
            position_id: Some("p".into()),
            token_mint: Some("T".into()),
            base_mint: Some("SOL".into()),
            entry_input_amount_atomic: Some(1_000_000),
            entry_output_amount_atomic: Some(1_000_000),
            token_decimals: Some(6),
            base_mint_decimals: Some(9),
            entry_fees_usd: Some(Decimal::ZERO),
            entry_slippage_bps: Some(0),
            entry_cost_model: None,
            quantity: dec!(1_000_000),
            remaining_quantity_atomic: Some(remaining),
            entry_cost_usd: Some(dec!(10)),
            base_entry_price_usd: Some(base_price),
            state: PositionState::Open,
            reconciliation_status: ReconciliationStatus::Reconciled,
            last_reconciled_at: None,
            exit_signature: None,
            exit_fees_usd: None,
            exit_time: None,
            entry_price_usd: dec!(0.00001),
            entry_time: Utc::now(),
            entry_signature: "sig".into(),
            high_water_price_usd: dec!(0.00001),
            realized_pnl_usd: Decimal::ZERO,
            unrealized_pnl_usd: Decimal::ZERO,
            fees_usd: Decimal::ZERO,
            current_value_usd: dec!(10),
            signal_id: "s".into(),
            exit_reason: None,
        }
    }
    #[test]
    fn mark_price_derives_from_quote_and_base_price() {
        // 1_000_000 tokens (6dp) remaining; sell quote returns 0.5 SOL (9dp);
        // base (SOL) priced at 150 USD -> token mark = 75 USD per token.
        let mark = mark_price_from_quote(500_000_000, 9, dec!(150), 1_000_000, 6).unwrap();
        assert_eq!(mark, dec!(75));
        assert!(mark_price_from_quote(0, 9, dec!(150), 1_000_000, 6).is_none());
        assert!(mark_price_from_quote(500_000_000, 9, dec!(150), 0, 6).is_none());
    }
    #[test]
    fn exit_keys_differ_per_attempt_and_reason() {
        assert_ne!(
            exit_idempotency_key("p", 100, "stop_loss", 0),
            exit_idempotency_key("p", 100, "stop_loss", 1)
        );
        assert_ne!(
            exit_idempotency_key("p", 100, "stop_loss", 0),
            exit_idempotency_key("p", 99, "stop_loss", 0)
        );
        assert_eq!(
            exit_idempotency_key("p", 100, "stop_loss", 0),
            exit_idempotency_key("p", 100, "stop_loss", 0)
        );
    }
    #[test]
    fn equity_sums_realized_and_unrealized() {
        let p1 = position(1_000, dec!(150));
        let mut p2 = position(2_000, dec!(150));
        p2.realized_pnl_usd = dec!(-2);
        p2.unrealized_pnl_usd = dec!(5);
        assert_eq!(equity_usd(dec!(100), &[&p1, &p2]), dec!(103));
    }
    #[test]
    fn exit_reason_with_invalidated_false_evaluates_exit_conditions() {
        let p = position(1_000, dec!(0.00001));
        let now = Utc::now();
        let strat = strategy_config();
        let reason = exit_reason(
            &p,
            dec!(0.000005),
            Decimal::MAX,
            dec!(50000),
            false,
            now,
            &strat,
        );
        assert_eq!(reason, Some(ExitReason::StopLoss));
    }

    struct MockExecutor;
    #[async_trait::async_trait]
    impl Executor for MockExecutor {
        async fn quote(
            &self,
            _input: &str,
            _output: &str,
            amount: u64,
            _slippage: u16,
        ) -> Result<Quote, ExecutionError> {
            Ok(Quote {
                input_mint: "SOL".into(),
                output_mint: "T".into(),
                input_amount: amount,
                output_amount: 5_000_000u64,
                price_impact_bps: 10,
                route: serde_json::json!({}),
                observed_at: Utc::now(),
            })
        }
        async fn execute(
            &self,
            _r: ExecutionRequest,
        ) -> Result<crate::domain::trade::Fill, ExecutionError> {
            Err(ExecutionError::Unavailable("mock".into()))
        }
        async fn health(&self) -> Result<(), ExecutionError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn process_exits_with_empty_candidate_queue_runs_successfully() {
        let config = test_config();
        let mock_executor = Arc::new(MockExecutor);
        let store = Arc::new(crate::storage::StateStore::open(":memory:").unwrap());
        let mut portfolio = Portfolio::default();
        let p = position(1_000, dec!(0.00001));
        let stored = vec![p];
        portfolio.load(stored.clone());
        store
            .save_position(portfolio.position("T").unwrap())
            .unwrap();
        let mut risk = RiskEngine::new(config.risk.clone(), config.risk.starting_capital_usd);
        risk.state.open_positions = 1;
        let mut state = SessionState {
            portfolio,
            risk,
            candidates: HashMap::new(),
            seen_signals: HashSet::new(),
            day_key: Utc::now().date_naive().to_string(),
            emergency_active: false,
        };
        let rpc = Arc::new(
            crate::data::rpc::RpcPool::with_attempts(
                vec!["http://localhost:1".into()],
                Duration::from_secs(1),
                1,
            )
            .unwrap(),
        );
        let deps = SessionDeps {
            config,
            store,
            executor: mock_executor,
            rpc,
        };
        let result = process_exits(&deps, &mut state).await;
        assert!(
            result.is_ok(),
            "process_exits should succeed with empty candidate queue"
        );
    }
    #[test]
    fn day_rollover_detects_new_utc_day() {
        assert!(day_rollover_check(Utc::now(), "2000-01-01"));
        assert!(!day_rollover_check(
            Utc::now(),
            &Utc::now().date_naive().to_string()
        ));
    }
}
