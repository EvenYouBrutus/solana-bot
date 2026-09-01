use crate::backtest::data::HistoricalSignal;
use crate::backtest::split::Split;
use crate::config::types::Config;
use crate::domain::position::{Position, PositionState, ReconciliationStatus};
use crate::domain::signal::{SignalScore, TradeSignal};
use crate::domain::token::TokenSafety;
use crate::domain::wallet::{Side, WalletStats};
use crate::economics::{EconomicGate, EconomicGateDecision, ExpectedValue};
use crate::strategy::exit::{exit_reason, ExitReason};
use crate::strategy::signal::StrategyDecision;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Whether execution costs are modeled assumptions or observed from real fills.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum CostMode {
    /// All costs are modeled assumptions (slippage, fees, impact).
    Modeled,
    /// All costs are observed from real execution fills.
    Observed,
    /// Mix of modeled and observed costs.
    Mixed,
}

impl std::fmt::Display for CostMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CostMode::Modeled => write!(f, "modeled"),
            CostMode::Observed => write!(f, "observed"),
            CostMode::Mixed => write!(f, "mixed"),
        }
    }
}

/// Modeled execution costs for a single trade leg.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeCosts {
    pub swap_fee_usd: Decimal,
    pub priority_fee_usd: Decimal,
    pub slippage_cost_usd: Decimal,
    pub price_impact_cost_usd: Decimal,
    pub total_usd: Decimal,
    /// Whether these costs are observed from real fills or modeled assumptions.
    pub is_observed: bool,
}

/// Generate a deterministic trade ID from signal data and index.
fn deterministic_trade_id(signal: &HistoricalSignal, index: usize) -> String {
    let mut hasher = DefaultHasher::new();
    signal.signal_timestamp.hash(&mut hasher);
    signal.mint.hash(&mut hasher);
    index.hash(&mut hasher);
    format!("bt:{:016x}", hasher.finish())
}

/// Complete record of a simulated trade.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulatedTrade {
    pub trade_id: String,
    pub signal_timestamp: DateTime<Utc>,
    pub mint: String,
    pub split: Split,

    // Entry
    pub entry_time: DateTime<Utc>,
    pub entry_price_usd: Decimal,
    pub position_usd: Decimal,
    pub entry_quantity_tokens: Decimal,
    pub entry_costs: TradeCosts,

    // Exit
    pub exit_time: DateTime<Utc>,
    pub exit_price_usd: Decimal,
    pub exit_reason: ExitReason,
    pub holding_minutes: i64,
    pub exit_costs: TradeCosts,

    // PnL
    pub gross_return_pct: Decimal,
    pub gross_pnl_usd: Decimal,
    pub total_cost_usd: Decimal,
    pub net_return_pct: Decimal,
    pub net_pnl_usd: Decimal,

    // Risk metrics
    pub mfe_pct: Decimal,
    pub mae_pct: Decimal,

    // Flags
    pub is_ambiguous: bool,
    pub ambiguous_reason: Option<String>,
    /// True when the price history was insufficient to determine a valid exit.
    pub is_censored: bool,
    /// Explanation of why the trade is censored (e.g. "insufficient future data").
    pub censored_reason: Option<String>,
    /// Whether costs are modeled, observed, or mixed.
    pub cost_mode: CostMode,
}

/// Backtest-specific configuration for cost modeling.
///
/// DOCUMENTED COST MODEL (single, internally consistent choice):
/// - Token quantity is `notional / observed_price` and is NEVER adjusted for
///   execution effects.
/// - Swap fees, network/priority fees, slippage, and price impact are each
///   represented EXACTLY ONCE, as a dollar cost per leg. They are never also
///   applied as a quantity reduction or as an adjustment to the entry/exit
///   price, so no adverse effect is double-counted.
/// - Failed transactions are a PROBABILISTIC expectation
///   (`2 * failed_tx_rate * failed_tx_cost_usd`), never a certain per-trade
///   cost.
/// - These assumptions are MODELED, not observed: every leg reports
///   `is_observed = false` and the trade's `cost_mode` is `CostMode::Modeled`.
#[derive(Debug, Clone, Deserialize)]
pub struct CostAssumptions {
    /// Entry leg: AMM/DEX swap fee in bps.
    pub entry_swap_fee_bps: Decimal,
    /// Entry leg: network priority fee in USD.
    pub entry_priority_fee_usd: Decimal,
    /// Entry leg: realized slippage in bps.
    pub entry_slippage_bps: Decimal,
    /// Entry leg: price impact in bps.
    pub entry_price_impact_bps: Decimal,
    /// Exit leg: AMM/DEX swap fee in bps.
    pub exit_swap_fee_bps: Decimal,
    /// Exit leg: network priority fee in USD.
    pub exit_priority_fee_usd: Decimal,
    /// Exit leg: realized slippage in bps.
    pub exit_slippage_bps: Decimal,
    /// Exit leg: price impact in bps.
    pub exit_price_impact_bps: Decimal,
    /// Expected probability of a failed transaction attempt (modeled).
    pub failed_tx_rate: Decimal,
    /// Cost per failed transaction in USD (modeled).
    pub failed_tx_cost_usd: Decimal,
}

impl CostAssumptions {
    /// Which `CostMode` these assumptions produce. Modeled assumptions only:
    /// observed and mixed modes require real fill data and are not
    /// constructible from assumption config.
    pub fn mode(&self) -> CostMode {
        CostMode::Modeled
    }

    /// Compute entry costs for a given notional USD amount.
    ///
    /// Modeled only. Each of swap fee, priority fee, slippage, and price
    /// impact appears exactly once as a dollar cost; the token quantity is
    /// never reduced and the entry price is never adjusted.
    pub fn entry_costs(&self, notional_usd: Decimal) -> TradeCosts {
        let swap_fee = notional_usd * self.entry_swap_fee_bps / dec!(10000);
        let slippage = notional_usd * self.entry_slippage_bps / dec!(10000);
        let impact = notional_usd * self.entry_price_impact_bps / dec!(10000);
        let total = swap_fee + self.entry_priority_fee_usd + slippage + impact;
        TradeCosts {
            swap_fee_usd: swap_fee,
            priority_fee_usd: self.entry_priority_fee_usd,
            slippage_cost_usd: slippage,
            price_impact_cost_usd: impact,
            total_usd: total,
            is_observed: false,
        }
    }

    /// Compute exit costs for a given proceeds USD amount.
    ///
    /// Modeled only, mirroring `entry_costs`: each effect exactly once as a
    /// dollar cost on the gross proceeds; no quantity or price adjustment.
    pub fn exit_costs(&self, proceeds_usd: Decimal) -> TradeCosts {
        let swap_fee = proceeds_usd * self.exit_swap_fee_bps / dec!(10000);
        let slippage = proceeds_usd * self.exit_slippage_bps / dec!(10000);
        let impact = proceeds_usd * self.exit_price_impact_bps / dec!(10000);
        let total = swap_fee + self.exit_priority_fee_usd + slippage + impact;
        TradeCosts {
            swap_fee_usd: swap_fee,
            priority_fee_usd: self.exit_priority_fee_usd,
            slippage_cost_usd: slippage,
            price_impact_cost_usd: impact,
            total_usd: total,
            is_observed: false,
        }
    }

    /// Expected cost of failed transactions across both legs.
    ///
    /// This is a PROBABILISTIC expected cost: `2 * failed_tx_rate * cost_per_tx`.
    /// It represents the average gas fees lost to failed submissions, NOT a
    /// guaranteed cost that definitely occurs on every trade.
    pub fn expected_failed_tx_cost(&self) -> Decimal {
        dec!(2) * self.failed_tx_rate * self.failed_tx_cost_usd
    }

    /// Total modeled round-trip cost for a position.
    pub fn total_round_trip_cost(
        &self,
        position_usd: Decimal,
        exit_proceeds_usd: Decimal,
    ) -> Decimal {
        self.entry_costs(position_usd).total_usd
            + self.exit_costs(exit_proceeds_usd).total_usd
            + self.expected_failed_tx_cost()
    }
}

/// Detect ambiguous OHLC situations where both SL and TP thresholds are
/// crossed between consecutive observations and ordering cannot be determined.
///
/// Conservative rule: when ambiguous, the interval is marked ambiguous and
/// the trade is excluded from performance statistics. We never choose the
/// favorable outcome.
fn detect_ambiguity(
    prev_price: Decimal,
    curr_price: Decimal,
    entry_price: Decimal,
    sl_pct: Decimal,
    tp_pct: Decimal,
) -> Option<String> {
    if entry_price <= Decimal::ZERO {
        return None;
    }
    let sl_price = entry_price * (dec!(1) - sl_pct / dec!(100));
    let tp_price = entry_price * (dec!(1) + tp_pct / dec!(100));
    let range_min = prev_price.min(curr_price);
    let range_max = prev_price.max(curr_price);
    if range_min <= sl_price && range_max >= tp_price {
        Some(format!(
            "SL ({sl_price}) and TP ({tp_price}) both within observed range [{range_min}, {range_max}]"
        ))
    } else {
        None
    }
}

/// Build a synthetic Position for exit_reason() evaluation.
fn build_position(
    signal: &HistoricalSignal,
    entry_price: Decimal,
    entry_time: DateTime<Utc>,
) -> Position {
    Position {
        mint: signal.mint.clone(),
        position_id: Some(format!("bt-pos:{}", signal.mint)),
        token_mint: Some(signal.mint.clone()),
        base_mint: None,
        entry_input_amount_atomic: None,
        entry_output_amount_atomic: None,
        token_decimals: Some(signal.token_decimals),
        base_mint_decimals: Some(signal.base_mint_decimals),
        entry_fees_usd: None,
        entry_slippage_bps: None,
        entry_cost_model: Some(signal.costs.clone()),
        quantity: dec!(1),
        remaining_quantity_atomic: Some(1),
        entry_cost_usd: Some(signal.position_usd),
        base_entry_price_usd: None,
        state: PositionState::Open,
        reconciliation_status: ReconciliationStatus::Reconciled,
        last_reconciled_at: Some(entry_time),
        exit_signature: None,
        exit_fees_usd: None,
        exit_time: None,
        entry_price_usd: entry_price,
        entry_time,
        entry_signature: format!("backtest:{}", signal.mint),
        high_water_price_usd: entry_price,
        realized_pnl_usd: Decimal::ZERO,
        unrealized_pnl_usd: Decimal::ZERO,
        fees_usd: Decimal::ZERO,
        current_value_usd: signal.position_usd,
        signal_id: signal.mint.clone(),
        exit_reason: None,
    }
}

/// Simulate a single historical signal through the full entry/exit pipeline.
///
/// Key invariants:
/// - `expected_gross_return_pct` from the signal is NEVER used in the entry decision.
/// - The entry decision uses the ACTUAL production thresholds from `config`
///   (economic edge, signal score, cost gate). There are no backtest overrides.
/// - All decision data is strictly point-in-time: market/safety/costs
///   observed_at and every wallet updated_at must be <= signal_timestamp.
/// - The expected return required by the production economic gate is
///   reconstructed ONLY from the point-in-time cost model (the average gross
///   win required to break even under the recorded payoff assumptions).
///   If it cannot be reconstructed, the signal is rejected as insufficient
///   economic evidence.
/// - Entry quantity is computed at market price; execution effects (slippage,
///   impact, fees) are captured as modeled dollar costs, not quantity reductions.
/// - MFE/MAE are computed from all observations up to and including the exit.
/// - The trailing stop evaluates against high-water from prior observations
///   (the current observation's price is evaluated against the previous high).
/// - TimeLimit is ONLY ever produced by exit_reason() evaluating a real
///   observation. If the walk ends with no trigger, the trade is Censored
///   (insufficient future data), never a synthetic TimeLimit.
/// - Ambiguous intervals (SL and TP both crossed, ordering unknowable) take
///   the documented conservative rule: exit at the stop level, flag the trade
///   ambiguous, exclude it from performance statistics.
/// - IDs are deterministic (no randomness; no wall-clock input anywhere).
pub fn simulate_signal(
    signal: &HistoricalSignal,
    config: &Config,
    cost_assumptions: &CostAssumptions,
    split: Split,
    trade_index: usize,
) -> Result<SimulatedTrade, String> {
    // --- Point-in-time validation of the economic inputs ---
    // The production entry path evaluates the economic gate on the cost model,
    // so the cost model must be point-in-time valid. Reject otherwise: cost
    // data observed after the signal is insufficient economic evidence.
    if signal.costs.observed_at > signal.signal_timestamp {
        return Err(
            "insufficient economic evidence: costs.observed_at is after signal_timestamp".into(),
        );
    }

    // --- Production economic cost gate (mirrors risk::authorize_entry) ---
    let gate = EconomicGate {
        round_trip_cost_threshold_pct: config.economics.round_trip_cost_threshold_pct,
    };
    let cost_result = match gate.check(&signal.costs) {
        Ok(EconomicGateDecision::Allowed(result)) => result,
        Ok(EconomicGateDecision::Rejected {
            result,
            threshold_pct,
        }) => {
            return Err(format!(
                "round-trip cost {}% exceeds {}% threshold",
                result.round_trip_cost_pct_of_position, threshold_pct
            ));
        }
        Err(e) => {
            return Err(format!("insufficient economic evidence: {e}"));
        }
    };

    // --- Point-in-time entry decision ---
    // The production economic gate needs an expected gross return. Production
    // derives it from a forward-looking forecast that is not reconstructable
    // from the historical record (and the recorded expected_gross_return_pct
    // must not influence the decision), so the expected return is reconstructed
    // purely from the point-in-time cost model: the average gross win required
    // to break even under the recorded payoff assumptions. If the cost model
    // cannot produce it, the signal lacks sufficient economic evidence.
    let expected = ExpectedValue::estimate(
        cost_result.required_avg_win_pct,
        &signal.costs,
        dec!(0),
        dec!(0),
        config.economics.uncertainty_haircut_pct,
    )
    .map_err(|e| format!("insufficient economic evidence: {e}"))?;

    let wallet_refs: Vec<&WalletStats> = signal.wallets.iter().collect();

    let decision = evaluate_signal_pit(
        config,
        &signal.mint,
        &wallet_refs,
        &signal.market,
        &signal.safety,
        &expected,
        signal.signal_timestamp,
    );

    let _signal_data = match decision {
        StrategyDecision::Accepted(s) => s,
        StrategyDecision::Rejected(reason) => {
            return Err(reason);
        }
    };

    // --- Entry simulation ---
    let entry_price = signal.market.price_usd;
    if entry_price <= Decimal::ZERO {
        return Err("non-positive entry price".into());
    }

    let entry_costs = cost_assumptions.entry_costs(signal.position_usd);

    // Entry quantity: full notional at market price. Execution effects
    // (slippage, impact, fees) are captured in entry_costs, NOT as a
    // quantity reduction. This avoids double-counting.
    let entry_quantity_tokens = signal.position_usd / entry_price;
    let entry_time = signal.signal_timestamp;

    // --- Walk price history for exit ---
    let mut position = build_position(signal, entry_price, entry_time);
    let mut high_water = entry_price;
    let mut exit_price = entry_price;
    let mut exit_time = entry_time;
    let mut exit_reason_found: Option<ExitReason> = None;
    let mut mfe = Decimal::ZERO;
    let mut mae = Decimal::ZERO;
    let mut is_ambiguous = false;
    let mut ambiguous_reason: Option<String> = None;

    for (i, obs) in signal.price_history.iter().enumerate() {
        let price = obs.price_usd;

        // Step 1: Check OHLC ambiguity (uses prev_price and current_price).
        // Ambiguity is detected BEFORE exit evaluation because it determines
        // whether the interval's outcome is knowable.
        if i > 0 {
            let prev_price = signal.price_history[i - 1].price_usd;
            if let Some(reason) = detect_ambiguity(
                prev_price,
                price,
                entry_price,
                config.strategy.stop_loss_pct,
                config.strategy.take_profit_pct,
            ) {
                is_ambiguous = true;
                ambiguous_reason = Some(reason);
                // Conservative rule: assume the unfavorable outcome (the stop)
                // fired inside the interval. Book the exit at the stop level,
                // never at the observed close, which may sit on the favorable
                // (take-profit) side of the interval.
                exit_price = entry_price * (dec!(1) - config.strategy.stop_loss_pct / dec!(100));
                exit_time = obs.timestamp;
                exit_reason_found = Some(ExitReason::StopLoss);
                break;
            }
        }

        // Step 2: Update MFE/MAE BEFORE exit evaluation.
        // This ensures the exit observation is included in MFE/MAE.
        let return_pct = (price - entry_price) / entry_price * dec!(100);
        if return_pct > mfe {
            mfe = return_pct;
        }
        if return_pct < mae {
            mae = return_pct;
        }

        // Step 3: Evaluate exit conditions.
        // The trailing stop checks price vs high_water from PRIOR observations.
        // We update high_water AFTER the exit check to ensure the trailing stop
        // evaluates against the previous high, not the current candle.
        let reason = exit_reason(
            &position,
            price,
            Some(obs.liquidity_usd),
            config.risk.min_liquidity_usd,
            false,
            obs.timestamp,
            &config.strategy,
        );

        if let Some(reason) = reason {
            exit_price = price;
            exit_time = obs.timestamp;
            exit_reason_found = Some(reason);
            break;
        }

        // Step 4: Update high-water mark AFTER exit evaluation.
        // This ensures trailing stop at next observation uses this obs's price
        // as the high-water only if it wasn't the exit candle.
        if price > high_water {
            high_water = price;
            position.high_water_price_usd = price;
        }
    }

    // --- Determine exit reason ---
    // TimeLimit is ONLY ever produced by exit_reason() evaluating a real
    // observation at/after max_holding_minutes. If the walk ended without a
    // trigger, the historical record ends before a valid exit can be
    // determined and there is no terminal event: the trade is Censored and
    // must never be reported as a synthetic TimeLimit exit.
    let (exit_reason_final, is_censored, censored_reason) = match &exit_reason_found {
        Some(reason) => (reason.clone(), false, None),
        None => {
            let span_minutes = signal
                .price_history
                .last()
                .map(|o| (o.timestamp - signal.signal_timestamp).num_minutes())
                .unwrap_or(0);
            (
                ExitReason::Censored,
                true,
                Some(format!(
                    "insufficient_future_data: {span_minutes} minutes of history after entry \
                     but max_holding is {} minutes; no exit trigger and no terminal event observed",
                    config.strategy.max_holding_minutes
                )),
            )
        }
    };

    // Set exit_price and exit_time for the no-trigger case. These mark where
    // the observable data ends; censored trades are excluded from realized
    // performance statistics.
    if exit_reason_found.is_none() {
        if let Some(last_obs) = signal.price_history.last() {
            exit_price = last_obs.price_usd;
            exit_time = last_obs.timestamp;
        }
    }

    let holding_minutes = (exit_time - entry_time).num_minutes();

    // --- Exit cost simulation ---
    // Exit proceeds: token quantity (never adjusted) * observed exit price.
    // Slippage and impact are dollar costs in exit_costs, NOT quantity
    // reductions or price adjustments — the same single-representation rule
    // as the entry leg, so no effect is double-counted.
    let exit_proceeds_gross = entry_quantity_tokens * exit_price;
    let exit_costs = cost_assumptions.exit_costs(exit_proceeds_gross);
    // Probabilistic expectation across both legs; NOT a certain per-trade cost.
    let failed_tx = cost_assumptions.expected_failed_tx_cost();
    let total_cost = entry_costs.total_usd + exit_costs.total_usd + failed_tx;

    // --- PnL ---
    // gross_pnl = quantity * exit_price - position_usd (market outcome only,
    // unaffected by any modeled cost).
    // total_cost = entry leg (swap + priority + slippage + impact)
    //            + exit leg (swap + priority + slippage + impact)
    //            + expected failed-tx cost (each term counted exactly once).
    // net_pnl = gross_pnl - total_cost.
    // returns are percentages of position_usd.
    let gross_pnl = exit_proceeds_gross - signal.position_usd;
    let net_pnl = gross_pnl - total_cost;
    let gross_return_pct = if signal.position_usd > Decimal::ZERO {
        gross_pnl / signal.position_usd * dec!(100)
    } else {
        Decimal::ZERO
    };
    let net_return_pct = if signal.position_usd > Decimal::ZERO {
        net_pnl / signal.position_usd * dec!(100)
    } else {
        Decimal::ZERO
    };

    Ok(SimulatedTrade {
        trade_id: deterministic_trade_id(signal, trade_index),
        signal_timestamp: signal.signal_timestamp,
        mint: signal.mint.clone(),
        split,
        entry_time,
        entry_price_usd: entry_price,
        position_usd: signal.position_usd,
        entry_quantity_tokens,
        entry_costs,
        exit_time,
        exit_price_usd: exit_price,
        exit_reason: exit_reason_final,
        holding_minutes,
        exit_costs,
        gross_return_pct,
        gross_pnl_usd: gross_pnl,
        total_cost_usd: total_cost,
        net_return_pct,
        net_pnl_usd: net_pnl,
        mfe_pct: mfe,
        mae_pct: mae,
        is_ambiguous,
        ambiguous_reason,
        is_censored,
        censored_reason,
        cost_mode: cost_assumptions.mode(),
    })
}

/// Point-in-time version of evaluate_signal that accepts an explicit `now`
/// parameter instead of using `Utc::now()`. Faithfully reproduces the same
/// validation order and thresholds from the production `evaluate_signal`,
/// using the ACTUAL production config values (no backtest overrides).
fn evaluate_signal_pit(
    config: &Config,
    mint: &str,
    wallets: &[&WalletStats],
    market: &MarketSnapshot,
    safety: &TokenSafety,
    expected: &ExpectedValue,
    now: DateTime<Utc>,
) -> StrategyDecision {
    // Staleness / future-dated check
    if market.observed_at > now
        || safety.observed_at > now
        || market.age_seconds(now) > config.rpc.max_data_age_secs
    {
        return StrategyDecision::Rejected("stale or future-dated market data".into());
    }
    // Liquidity
    if market.liquidity_usd < config.risk.min_liquidity_usd {
        return StrategyDecision::Rejected("insufficient liquidity".into());
    }
    // Token safety
    if safety.token_age_secs < config.strategy.min_token_age_secs
        || safety.mint_authority_present
        || safety.freeze_authority_present
        || safety.holder_top10_pct > dec!(70)
        || safety.sellable != Some(true)
        || safety.route_available != Some(true)
        || safety.creator_suspicious == Some(true)
        || safety.abnormal_activity == Some(true)
        || safety.liquidity_change_pct.unwrap_or(Decimal::ZERO) < dec!(-20)
    {
        return StrategyDecision::Rejected(
            "token safety filter or incomplete safety evidence".into(),
        );
    }
    // Wallet consensus
    if wallets.len() < config.strategy.min_consensus_wallets {
        return StrategyDecision::Rejected(
            "insufficient independent qualified wallet consensus".into(),
        );
    }
    // Wallet quality
    if wallets.iter().any(|w| {
        w.trades < config.strategy.min_wallet_samples
            || w.score < config.strategy.min_wallet_score
            || w.updated_at > now
    }) {
        return StrategyDecision::Rejected(
            "wallet evidence below configured confidence threshold".into(),
        );
    }
    // Economic edge — production threshold, no override. Uses the expected
    // value reconstructed from point-in-time cost data, NOT
    // signal.expected_gross_return_pct.
    if expected.net_return_pct < config.economics.min_expected_net_return_pct {
        return StrategyDecision::Rejected("economic edge below threshold".into());
    }
    // Signal scoring
    let count = Decimal::from(wallets.len() as u32);
    let mut score = SignalScore {
        wallet_score: wallets.iter().map(|w| w.score).sum::<Decimal>() / count,
        wallet_sample_size: wallets.iter().map(|w| w.trades).min().unwrap_or(0),
        wallet_recent_score: wallets.iter().map(|w| w.recent_return_pct).sum::<Decimal>() / count,
        consensus_score: (count * dec!(20)).min(dec!(100)),
        liquidity_score: (market.liquidity_usd / dec!(10000) * dec!(100)).min(dec!(100)),
        momentum_score: (market.buy_sell_imbalance * dec!(50)).clamp(Decimal::ZERO, dec!(100)),
        risk_score: dec!(100) - market.volatility_pct.min(dec!(100)),
        economic_score: (expected.net_return_pct * dec!(10)).min(dec!(100)),
        final_signal_score: Decimal::ZERO,
    };
    score.final_signal_score = (score.wallet_score
        + score.consensus_score
        + score.liquidity_score
        + score.momentum_score
        + score.risk_score
        + score.economic_score)
        / dec!(6);
    if score.final_signal_score < config.strategy.min_signal_score {
        return StrategyDecision::Rejected("signal confidence below threshold".into());
    }
    StrategyDecision::Accepted(Box::new(TradeSignal {
        id: deterministic_trade_id_from_str(mint, now),
        mint: mint.into(),
        wallets: wallets.iter().map(|w| w.wallet.clone()).collect(),
        side: Side::Buy,
        score,
        expected_gross_return_pct: expected.gross_return_pct,
        created_at: now,
        reason: "qualified-wallet accumulation with liquid safe market".into(),
    }))
}

fn deterministic_trade_id_from_str(s: &str, ts: DateTime<Utc>) -> String {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    ts.hash(&mut hasher);
    format!("sig:{:016x}", hasher.finish())
}

use crate::domain::market::MarketSnapshot;

/// Backtest-specific configuration loaded from `config/backtest.toml`.
///
/// This config intentionally contains NO strategy/economic threshold
/// overrides: the entry decision always uses the production thresholds
/// from the main `Config` (economic edge, signal score, cost gate).
#[derive(Debug, Clone, Deserialize)]
pub struct BacktestConfig {
    pub split: crate::backtest::split::SplitConfig,
    #[serde(default)]
    pub costs: BacktestCostConfig,
    /// Starting capital in USD for drawdown calculations.
    #[serde(default = "default_capital")]
    pub capital_usd: rust_decimal::Decimal,
}

fn default_capital() -> rust_decimal::Decimal {
    rust_decimal_macros::dec!(100)
}

/// Cost assumptions in the backtest TOML config.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BacktestCostConfig {
    pub entry_swap_fee_bps: Option<rust_decimal::Decimal>,
    pub entry_priority_fee_usd: Option<rust_decimal::Decimal>,
    pub entry_slippage_bps: Option<rust_decimal::Decimal>,
    pub entry_price_impact_bps: Option<rust_decimal::Decimal>,
    pub exit_swap_fee_bps: Option<rust_decimal::Decimal>,
    pub exit_priority_fee_usd: Option<rust_decimal::Decimal>,
    pub exit_slippage_bps: Option<rust_decimal::Decimal>,
    pub exit_price_impact_bps: Option<rust_decimal::Decimal>,
    pub failed_tx_rate: Option<rust_decimal::Decimal>,
    pub failed_tx_cost_usd: Option<rust_decimal::Decimal>,
}

impl CostAssumptions {
    /// Build `CostAssumptions` from backtest config, using defaults for missing fields.
    pub fn from_config(bt: &BacktestConfig) -> Self {
        let c = &bt.costs;
        CostAssumptions {
            entry_swap_fee_bps: c
                .entry_swap_fee_bps
                .unwrap_or(rust_decimal_macros::dec!(30)),
            entry_priority_fee_usd: c
                .entry_priority_fee_usd
                .unwrap_or(rust_decimal_macros::dec!(0.002)),
            entry_slippage_bps: c
                .entry_slippage_bps
                .unwrap_or(rust_decimal_macros::dec!(50)),
            entry_price_impact_bps: c
                .entry_price_impact_bps
                .unwrap_or(rust_decimal_macros::dec!(20)),
            exit_swap_fee_bps: c.exit_swap_fee_bps.unwrap_or(rust_decimal_macros::dec!(30)),
            exit_priority_fee_usd: c
                .exit_priority_fee_usd
                .unwrap_or(rust_decimal_macros::dec!(0.002)),
            exit_slippage_bps: c.exit_slippage_bps.unwrap_or(rust_decimal_macros::dec!(50)),
            exit_price_impact_bps: c
                .exit_price_impact_bps
                .unwrap_or(rust_decimal_macros::dec!(20)),
            failed_tx_rate: c.failed_tx_rate.unwrap_or(rust_decimal_macros::dec!(0.05)),
            failed_tx_cost_usd: c
                .failed_tx_cost_usd
                .unwrap_or(rust_decimal_macros::dec!(0.002)),
        }
    }
}

/// Full result returned by `backtest::run_backtest()`.
#[derive(Debug, Clone, Serialize)]
pub struct BacktestResult {
    pub statistics: crate::backtest::stats::BacktestStatistics,
    pub all_trades: Vec<SimulatedTrade>,
    pub total_signals: usize,
    pub accepted_trades: usize,
    pub rejected_count: usize,
}

impl std::fmt::Display for BacktestResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Requirement: the report must explicitly disclose that the sample
        // backtest prices execution with MODELED assumptions, not observed
        // fills. (The statistics block also prints the cost mode.)
        writeln!(
            f,
            "Execution costs: MODELED ASSUMPTIONS (swap fees, priority fees, slippage, \
             price impact, expected failed-tx cost) — not observed fills"
        )?;
        writeln!(f, "{}", self.statistics)?;
        writeln!(f, "Rejections: {}", self.rejected_count)?;
        writeln!(f, "Trades: {}", self.all_trades.len())?;
        Ok(())
    }
}

impl BacktestResult {
    pub fn to_json_summary(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(&self.statistics)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backtest::data::PriceObservation;
    use crate::economics::{BreakEvenInputs, CostModel};
    use rust_decimal_macros::dec;

    fn base_config() -> Config {
        // Mirrors the production thresholds in config/paper.toml. The backtest
        // entry decision must run under the ACTUAL production gates.
        let text = r#"
mode = "paper"
[rpc]
http_endpoints = ["https://api.test"]
max_data_age_secs = 15
[strategy]
base_mint = "So11111111111111111111111111111111111111112"
min_wallet_score = 60.0
min_wallet_samples = 25
min_consensus_wallets = 2
min_signal_score = 65.0
min_token_age_secs = 86400
stop_loss_pct = 5.0
take_profit_pct = 12.0
trailing_stop_pct = 4.0
max_holding_minutes = 240
[economics]
round_trip_cost_threshold_pct = 3.0
min_expected_net_return_pct = 2.0
max_quote_age_secs = 3
uncertainty_haircut_pct = 1.0
[risk]
starting_capital_usd = 100.0
max_live_capital_usd = 100.0
max_concurrent_positions = 5
max_position_percent_of_equity = 100.0
max_position_percent_of_liquidity = 100.0
max_risk_per_trade_percent = 100.0
max_daily_loss_percent = 100.0
max_total_drawdown_before_kill_switch_pct = 100.0
cooldown_after_loss_minutes = 0
max_slippage_bps = 10000
min_liquidity_usd = 50000.0
max_trades_per_day = 100
[execution]
provider = "jupiter"
jupiter_api_url = "https://api.jup.ag"
slippage_bps = 75
priority_fee_lamports = 10000
allowed_program_ids = []
[storage]
sqlite_path = ":memory:"
"#;
        toml::from_str(text).unwrap()
    }

    fn cost_assumptions() -> CostAssumptions {
        CostAssumptions {
            entry_swap_fee_bps: dec!(30),
            entry_priority_fee_usd: dec!(0.002),
            entry_slippage_bps: dec!(50),
            entry_price_impact_bps: dec!(20),
            exit_swap_fee_bps: dec!(30),
            exit_priority_fee_usd: dec!(0.002),
            exit_slippage_bps: dec!(50),
            exit_price_impact_bps: dec!(20),
            failed_tx_rate: dec!(0.05),
            failed_tx_cost_usd: dec!(0.002),
        }
    }

    fn cost_model() -> CostModel {
        CostModel {
            observed_at: "2024-01-15T12:00:00Z".parse().unwrap(),
            source: "test".into(),
            is_live_snapshot: false,
            input: BreakEvenInputs {
                position_size_usd: dec!(4),
                avg_priority_fee_usd: dec!(0.002),
                avg_swap_fee_bps: dec!(30),
                avg_slippage_bps: dec!(50),
                avg_price_impact_bps: dec!(20),
                failed_tx_rate: dec!(0.05),
                avg_failed_tx_cost_usd: dec!(0.002),
                assumed_win_loss_ratio: dec!(2),
                assumed_avg_loss_pct: dec!(10),
            },
        }
    }

    fn market_snapshot(now: &str, price: Decimal, liquidity: Decimal) -> MarketSnapshot {
        let ts: DateTime<Utc> = now.parse().unwrap();
        MarketSnapshot {
            mint: "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263".into(),
            price_usd: price,
            liquidity_usd: liquidity,
            volume_24h_usd: dec!(50000),
            volatility_pct: dec!(5),
            buy_sell_imbalance: dec!(1.0),
            observed_at: ts,
            received_at: ts,
            slot: None,
        }
    }

    fn wallet_stats(now: &str, score: Decimal) -> WalletStats {
        let ts: DateTime<Utc> = now.parse().unwrap();
        WalletStats {
            wallet: "wallet1".into(),
            entity_id: None,
            realized_pnl_usd: dec!(1000),
            win_rate: dec!(0.7),
            avg_return_pct: dec!(15),
            median_return_pct: dec!(12),
            max_drawdown_pct: dec!(20),
            trades: 50,
            recent_return_pct: dec!(10),
            concentration_pct: dec!(5),
            scam_exposure_pct: dec!(0),
            score,
            tier: crate::domain::wallet::WalletTier::Qualified,
            updated_at: ts,
        }
    }

    fn make_signal(
        signal_time: &str,
        price: Decimal,
        liquidity: Decimal,
        price_history: Vec<(Decimal, Decimal)>,
    ) -> HistoricalSignal {
        let now: DateTime<Utc> = signal_time.parse().unwrap();
        let history: Vec<PriceObservation> = price_history
            .into_iter()
            .enumerate()
            .map(|(i, (p, l))| PriceObservation {
                timestamp: now + chrono::Duration::minutes((i as i64 + 1) * 5),
                price_usd: p,
                liquidity_usd: l,
            })
            .collect();
        HistoricalSignal {
            signal_timestamp: now,
            mint: "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263".into(),
            market: market_snapshot(signal_time, price, liquidity),
            safety: TokenSafety {
                mint_authority_present: false,
                freeze_authority_present: false,
                holder_top10_pct: dec!(40),
                token_age_secs: 172800,
                liquidity_locked_or_burned: None,
                sellable: Some(true),
                route_available: Some(true),
                creator_suspicious: None,
                abnormal_activity: None,
                liquidity_change_pct: None,
                observed_at: now,
            },
            wallets: vec![
                wallet_stats(signal_time, dec!(80)),
                wallet_stats(signal_time, dec!(75)),
            ],
            costs: cost_model(),
            position_usd: dec!(4),
            expected_gross_return_pct: dec!(15),
            token_decimals: 6,
            base_mint_decimals: 9,
            price_history: history,
        }
    }

    #[test]
    fn take_profit_trade() {
        let config = base_config();
        let signal = make_signal(
            "2024-01-15T12:00:00Z",
            dec!(0.0001),
            dec!(100000),
            vec![(dec!(0.00011), dec!(100000)), (dec!(0.00012), dec!(100000))],
        );
        let trade =
            simulate_signal(&signal, &config, &cost_assumptions(), Split::Train, 0).unwrap();
        assert_eq!(trade.exit_reason, ExitReason::TakeProfit);
        assert!(trade.gross_pnl_usd > Decimal::ZERO);
        assert!(!trade.is_ambiguous);
    }

    #[test]
    fn stop_loss_trade() {
        let config = base_config();
        let signal = make_signal(
            "2024-01-15T12:00:00Z",
            dec!(0.0001),
            dec!(100000),
            vec![
                (dec!(0.000095), dec!(100000)),
                (dec!(0.00009), dec!(100000)),
            ],
        );
        let trade =
            simulate_signal(&signal, &config, &cost_assumptions(), Split::Train, 0).unwrap();
        assert_eq!(trade.exit_reason, ExitReason::StopLoss);
        assert!(trade.gross_pnl_usd < Decimal::ZERO);
    }

    #[test]
    fn trailing_stop_trade() {
        let config = base_config();
        let signal = make_signal(
            "2024-01-15T12:00:00Z",
            dec!(0.0001),
            dec!(100000),
            vec![
                (dec!(0.00011), dec!(100000)),
                (dec!(0.000105), dec!(100000)),
                (dec!(0.000101), dec!(100000)),
            ],
        );
        let trade =
            simulate_signal(&signal, &config, &cost_assumptions(), Split::Train, 0).unwrap();
        assert!(
            trade.exit_reason == ExitReason::TrailingStop
                || trade.exit_reason == ExitReason::TakeProfit
        );
    }

    #[test]
    fn time_limit_exit_with_sufficient_data() {
        let mut config = base_config();
        config.strategy.max_holding_minutes = 10;
        let signal = make_signal(
            "2024-01-15T12:00:00Z",
            dec!(0.0001),
            dec!(100000),
            vec![
                (dec!(0.000101), dec!(100000)),
                (dec!(0.000102), dec!(100000)),
                (dec!(0.000103), dec!(100000)),
            ],
        );
        // 3 obs at 5min intervals = 15 min of data >= max_holding of 10 min
        let trade =
            simulate_signal(&signal, &config, &cost_assumptions(), Split::Train, 0).unwrap();
        assert_eq!(trade.exit_reason, ExitReason::TimeLimit);
        assert!(!trade.is_censored);
    }

    #[test]
    fn censored_when_insufficient_data() {
        let mut config = base_config();
        config.strategy.max_holding_minutes = 240;
        let signal = make_signal(
            "2024-01-15T12:00:00Z",
            dec!(0.0001),
            dec!(100000),
            vec![
                (dec!(0.000101), dec!(100000)),
                (dec!(0.000102), dec!(100000)),
            ],
        );
        // 2 obs at 5min intervals = 10 min of data < max_holding of 240 min
        let trade =
            simulate_signal(&signal, &config, &cost_assumptions(), Split::Train, 0).unwrap();
        assert_eq!(trade.exit_reason, ExitReason::Censored);
        assert!(trade.is_censored);
        assert!(trade.censored_reason.is_some());
        assert!(trade
            .censored_reason
            .unwrap()
            .contains("insufficient_future_data"));
    }

    #[test]
    fn censored_not_time_limit() {
        let mut config = base_config();
        config.strategy.max_holding_minutes = 240;
        let signal = make_signal(
            "2024-01-15T12:00:00Z",
            dec!(0.0001),
            dec!(100000),
            vec![(dec!(0.000101), dec!(100000))],
        );
        let trade =
            simulate_signal(&signal, &config, &cost_assumptions(), Split::Train, 0).unwrap();
        assert_eq!(trade.exit_reason, ExitReason::Censored);
        assert!(trade.is_censored);
    }

    #[test]
    fn no_trigger_before_max_holding_is_censored_not_time_limit() {
        // Flat, untriggered price path whose history ends before
        // max_holding_minutes must be Censored, never a synthetic TimeLimit.
        let config = base_config(); // max_holding = 240
        let signal = make_signal(
            "2024-01-15T12:00:00Z",
            dec!(0.0001),
            dec!(100000),
            vec![(dec!(0.0001), dec!(100000)), (dec!(0.0001), dec!(100000))],
        );
        let trade =
            simulate_signal(&signal, &config, &cost_assumptions(), Split::Train, 0).unwrap();
        assert_eq!(trade.exit_reason, ExitReason::Censored);
        assert!(trade.is_censored);
        assert!(trade
            .censored_reason
            .unwrap()
            .contains("insufficient_future_data"));
    }

    #[test]
    fn time_limit_only_from_evaluated_exit_not_synthetic() {
        // With sufficient history, TimeLimit fires at the FIRST observation
        // at/after max_holding_minutes (+10 min here), not at the end of the
        // data (+15 min). This proves TimeLimit comes from exit evaluation.
        let mut config = base_config();
        config.strategy.max_holding_minutes = 10;
        let signal = make_signal(
            "2024-01-15T12:00:00Z",
            dec!(0.0001),
            dec!(100000),
            vec![
                (dec!(0.0001), dec!(100000)),
                (dec!(0.0001), dec!(100000)),
                (dec!(0.0001), dec!(100000)),
            ],
        );
        let trade =
            simulate_signal(&signal, &config, &cost_assumptions(), Split::Train, 0).unwrap();
        assert_eq!(trade.exit_reason, ExitReason::TimeLimit);
        assert!(!trade.is_censored);
        assert_eq!(trade.holding_minutes, 10);
    }

    #[test]
    fn liquidity_exit() {
        let mut config = base_config();
        config.risk.min_liquidity_usd = dec!(80000);
        let signal = make_signal(
            "2024-01-15T12:00:00Z",
            dec!(0.0001),
            dec!(100000),
            vec![(dec!(0.0001), dec!(70000))],
        );
        let trade =
            simulate_signal(&signal, &config, &cost_assumptions(), Split::Train, 0).unwrap();
        assert_eq!(trade.exit_reason, ExitReason::LiquidityDeterioration);
    }

    #[test]
    fn costs_are_modeled_not_observed() {
        let config = base_config();
        let signal = make_signal(
            "2024-01-15T12:00:00Z",
            dec!(0.0001),
            dec!(100000),
            vec![(dec!(0.00011), dec!(100000))],
        );
        let trade =
            simulate_signal(&signal, &config, &cost_assumptions(), Split::Train, 0).unwrap();
        assert!(!trade.entry_costs.is_observed);
        assert!(!trade.exit_costs.is_observed);
        assert_eq!(trade.cost_mode, CostMode::Modeled);
    }

    /// Symmetric assumptions across both legs for the hand-calculated tests.
    fn symmetric_ca(
        swap_fee_bps: Decimal,
        priority_fee_usd: Decimal,
        slippage_bps: Decimal,
        impact_bps: Decimal,
        failed_tx_rate: Decimal,
        failed_tx_cost_usd: Decimal,
    ) -> CostAssumptions {
        CostAssumptions {
            entry_swap_fee_bps: swap_fee_bps,
            entry_priority_fee_usd: priority_fee_usd,
            entry_slippage_bps: slippage_bps,
            entry_price_impact_bps: impact_bps,
            exit_swap_fee_bps: swap_fee_bps,
            exit_priority_fee_usd: priority_fee_usd,
            exit_slippage_bps: slippage_bps,
            exit_price_impact_bps: impact_bps,
            failed_tx_rate,
            failed_tx_cost_usd,
        }
    }

    /// Standard scenario for the hand-calculated tests: TP exit at +12%.
    /// position_usd = 4, entry 0.0001 → quantity 40000, exit 0.000112
    /// → gross proceeds 4.48, gross PnL +0.48 (+12%).
    fn tp_signal() -> HistoricalSignal {
        make_signal(
            "2024-01-15T12:00:00Z",
            dec!(0.0001),
            dec!(100000),
            vec![(dec!(0.000112), dec!(100000))],
        )
    }

    #[test]
    fn hand_calculated_pnl_with_zero_fees() {
        // All costs zero: net PnL must equal gross PnL exactly.
        let config = base_config();
        let ca = symmetric_ca(dec!(0), dec!(0), dec!(0), dec!(0), dec!(0), dec!(0));
        let trade = simulate_signal(&tp_signal(), &config, &ca, Split::Train, 0).unwrap();
        assert_eq!(trade.exit_reason, ExitReason::TakeProfit);
        assert_eq!(trade.entry_costs.total_usd, dec!(0));
        assert_eq!(trade.exit_costs.total_usd, dec!(0));
        assert_eq!(trade.total_cost_usd, dec!(0));
        assert_eq!(trade.gross_pnl_usd, dec!(0.48)); // 4.48 - 4
        assert_eq!(trade.net_pnl_usd, dec!(0.48));
        assert_eq!(trade.gross_return_pct, dec!(12)); // 0.48 / 4 * 100
        assert_eq!(trade.net_return_pct, dec!(12));
    }

    #[test]
    fn hand_calculated_pnl_with_nonzero_fees() {
        // Fixture assumptions: 30bps swap + 0.002 priority + 50bps slippage
        // + 20bps impact per leg; failed tx 5% at 0.002.
        // Entry leg: 4 * (30+50+20)bps = 0.04, + 0.002 = 0.042.
        // Exit leg: 4.48 * 100bps = 0.0448, + 0.002 = 0.0468.
        // Failed tx expectation: 2 * 0.05 * 0.002 = 0.0002.
        // total = 0.042 + 0.0468 + 0.0002 = 0.089.
        // net = 0.48 - 0.089 = 0.391 → 9.775% of 4.
        let config = base_config();
        let ca = symmetric_ca(
            dec!(30),
            dec!(0.002),
            dec!(50),
            dec!(20),
            dec!(0.05),
            dec!(0.002),
        );
        let trade = simulate_signal(&tp_signal(), &config, &ca, Split::Train, 0).unwrap();
        assert_eq!(trade.entry_costs.total_usd, dec!(0.042));
        assert_eq!(trade.exit_costs.total_usd, dec!(0.0468));
        assert_eq!(trade.total_cost_usd, dec!(0.089));
        assert_eq!(trade.gross_pnl_usd, dec!(0.48));
        assert_eq!(trade.net_pnl_usd, dec!(0.391));
        assert_eq!(trade.gross_return_pct, dec!(12));
        assert_eq!(trade.net_return_pct, dec!(9.775));
    }

    #[test]
    fn slippage_charged_exactly_once_as_dollar_cost() {
        // Only slippage: 50bps per leg, nothing else.
        // Entry: 4 * 50/10000 = 0.02. Exit: 4.48 * 50/10000 = 0.0224.
        // Quantity stays exactly position/price (no hidden second charge).
        let config = base_config();
        let ca = symmetric_ca(dec!(0), dec!(0), dec!(50), dec!(0), dec!(0), dec!(0));
        let signal = tp_signal();
        let trade = simulate_signal(&signal, &config, &ca, Split::Train, 0).unwrap();
        assert_eq!(trade.entry_costs.slippage_cost_usd, dec!(0.02));
        assert_eq!(trade.exit_costs.slippage_cost_usd, dec!(0.0224));
        assert_eq!(trade.total_cost_usd, dec!(0.0424));
        assert_eq!(
            trade.entry_quantity_tokens,
            signal.position_usd / dec!(0.0001)
        );
        assert_eq!(trade.gross_pnl_usd, dec!(0.48));
        assert_eq!(trade.net_pnl_usd, dec!(0.4376)); // 0.48 - 0.0424
    }

    #[test]
    fn price_impact_charged_exactly_once_as_dollar_cost() {
        // Only impact: 20bps per leg.
        // Entry: 4 * 20/10000 = 0.008. Exit: 4.48 * 20/10000 = 0.00896.
        let config = base_config();
        let ca = symmetric_ca(dec!(0), dec!(0), dec!(0), dec!(20), dec!(0), dec!(0));
        let trade = simulate_signal(&tp_signal(), &config, &ca, Split::Train, 0).unwrap();
        assert_eq!(trade.entry_costs.price_impact_cost_usd, dec!(0.008));
        assert_eq!(trade.exit_costs.price_impact_cost_usd, dec!(0.00896));
        assert_eq!(trade.total_cost_usd, dec!(0.01696));
        assert_eq!(trade.net_pnl_usd, dec!(0.46304)); // 0.48 - 0.01696
    }

    #[test]
    fn swap_fee_charged_on_notional_per_leg() {
        // Only swap fee: 30bps per leg.
        // Entry: 4 * 30/10000 = 0.012. Exit: 4.48 * 30/10000 = 0.01344.
        let config = base_config();
        let ca = symmetric_ca(dec!(30), dec!(0), dec!(0), dec!(0), dec!(0), dec!(0));
        let trade = simulate_signal(&tp_signal(), &config, &ca, Split::Train, 0).unwrap();
        assert_eq!(trade.entry_costs.swap_fee_usd, dec!(0.012));
        assert_eq!(trade.exit_costs.swap_fee_usd, dec!(0.01344));
        assert_eq!(trade.total_cost_usd, dec!(0.02544));
        assert_eq!(trade.net_pnl_usd, dec!(0.45456)); // 0.48 - 0.02544
    }

    #[test]
    fn priority_fee_is_fixed_usd_per_leg() {
        // Only priority fee: 0.002 per leg, independent of notional.
        let config = base_config();
        let ca = symmetric_ca(dec!(0), dec!(0.002), dec!(0), dec!(0), dec!(0), dec!(0));
        let trade = simulate_signal(&tp_signal(), &config, &ca, Split::Train, 0).unwrap();
        assert_eq!(trade.entry_costs.priority_fee_usd, dec!(0.002));
        assert_eq!(trade.exit_costs.priority_fee_usd, dec!(0.002));
        assert_eq!(trade.total_cost_usd, dec!(0.004));
        assert_eq!(trade.net_pnl_usd, dec!(0.476)); // 0.48 - 0.004
    }

    #[test]
    fn failed_tx_cost_is_probabilistic_expectation() {
        // 5% failure rate at 0.002 per failure, two legs:
        // E[cost] = 2 * 0.05 * 0.002 = 0.0002. This is the EXPECTED cost —
        // a 100%-failure rate would be needed for 2 * 0.002 per trade.
        let config = base_config();
        let ca = symmetric_ca(dec!(0), dec!(0), dec!(0), dec!(0), dec!(0.05), dec!(0.002));
        assert_eq!(ca.expected_failed_tx_cost(), dec!(0.0002));
        let trade = simulate_signal(&tp_signal(), &config, &ca, Split::Train, 0).unwrap();
        assert_eq!(trade.total_cost_usd, dec!(0.0002));
        assert_eq!(trade.net_pnl_usd, dec!(0.4798)); // 0.48 - 0.0002

        // Expectation scales with the rate: zero rate → zero cost.
        let zero_rate = symmetric_ca(dec!(0), dec!(0), dec!(0), dec!(0), dec!(0), dec!(0.002));
        assert_eq!(zero_rate.expected_failed_tx_cost(), dec!(0));
        // Certain failure on both legs would cost 2 * 0.002 = 0.004 — the
        // modeled 5% rate must NOT behave like that.
        let certain = symmetric_ca(dec!(0), dec!(0), dec!(0), dec!(0), dec!(1), dec!(0.002));
        assert_eq!(certain.expected_failed_tx_cost(), dec!(0.004));
    }

    #[test]
    fn complete_round_trip_cost_matches_leg_sum() {
        let ca = symmetric_ca(
            dec!(30),
            dec!(0.002),
            dec!(50),
            dec!(20),
            dec!(0.05),
            dec!(0.002),
        );
        // Entry 0.042 + exit 0.0468 + failed-tx 0.0002 = 0.089 (same as the
        // hand calculation in hand_calculated_pnl_with_nonzero_fees).
        assert_eq!(ca.total_round_trip_cost(dec!(4), dec!(4.48)), dec!(0.089));
        assert_eq!(ca.mode(), CostMode::Modeled);
    }

    #[test]
    fn net_pnl_includes_all_costs() {
        let config = base_config();
        let ca = cost_assumptions();
        let signal = make_signal(
            "2024-01-15T12:00:00Z",
            dec!(0.0001),
            dec!(100000),
            vec![(dec!(0.00011), dec!(100000))],
        );
        let trade = simulate_signal(&signal, &config, &ca, Split::Train, 0).unwrap();
        // Total cost = entry + exit + expected failed tx cost
        // No slippage_factor double-count: entry costs include slippage as
        // a dollar cost, quantity is at full market price.
        let expected_total_cost = ca.entry_costs(signal.position_usd).total_usd
            + ca.exit_costs(trade.gross_pnl_usd + signal.position_usd)
                .total_usd
            + ca.expected_failed_tx_cost();
        assert_eq!(trade.total_cost_usd, expected_total_cost);
        assert_eq!(
            trade.net_pnl_usd,
            trade.gross_pnl_usd - trade.total_cost_usd
        );
    }

    #[test]
    fn expected_gross_return_pct_does_not_affect_entry_decision() {
        // Regression test: changing expected_gross_return_pct alone cannot
        // change the historical entry decision.
        let config = base_config();
        let mut signal_low = make_signal(
            "2024-01-15T12:00:00Z",
            dec!(0.0001),
            dec!(100000),
            vec![(dec!(0.00011), dec!(100000))],
        );
        signal_low.expected_gross_return_pct = dec!(0);

        let mut signal_high = make_signal(
            "2024-01-15T12:00:00Z",
            dec!(0.0001),
            dec!(100000),
            vec![(dec!(0.00011), dec!(100000))],
        );
        signal_high.expected_gross_return_pct = dec!(9999);

        let result_low =
            simulate_signal(&signal_low, &config, &cost_assumptions(), Split::Train, 0);
        let result_high =
            simulate_signal(&signal_high, &config, &cost_assumptions(), Split::Train, 0);

        // Both must produce the same outcome: same exit reason, same PnL.
        assert_eq!(result_low.is_ok(), result_high.is_ok());
        if let (Ok(tl), Ok(th)) = (&result_low, &result_high) {
            assert_eq!(tl.exit_reason, th.exit_reason);
            assert_eq!(tl.gross_pnl_usd, th.gross_pnl_usd);
            assert_eq!(tl.net_pnl_usd, th.net_pnl_usd);
        }

        // Same invariance on the rejection path: with a signal score above
        // the production threshold, both variants are rejected identically.
        let mut strict = base_config();
        strict.strategy.min_signal_score = dec!(70.0);
        let err_low = simulate_signal(&signal_low, &strict, &cost_assumptions(), Split::Train, 0)
            .unwrap_err();
        let err_high = simulate_signal(&signal_high, &strict, &cost_assumptions(), Split::Train, 0)
            .unwrap_err();
        assert_eq!(err_low, err_high);
        assert!(err_low.contains("signal confidence below threshold"));
    }

    #[test]
    fn future_market_data_rejected_in_decision_path() {
        let config = base_config();
        let mut signal = make_signal(
            "2024-01-15T12:00:00Z",
            dec!(0.0001),
            dec!(100000),
            vec![(dec!(0.00011), dec!(100000))],
        );
        signal.market.observed_at = "2024-01-15T12:01:00Z".parse().unwrap();
        let err =
            simulate_signal(&signal, &config, &cost_assumptions(), Split::Train, 0).unwrap_err();
        assert!(err.contains("future-dated"), "unexpected error: {err}");
    }

    #[test]
    fn future_safety_data_rejected_in_decision_path() {
        let config = base_config();
        let mut signal = make_signal(
            "2024-01-15T12:00:00Z",
            dec!(0.0001),
            dec!(100000),
            vec![(dec!(0.00011), dec!(100000))],
        );
        signal.safety.observed_at = "2024-01-15T12:01:00Z".parse().unwrap();
        let err =
            simulate_signal(&signal, &config, &cost_assumptions(), Split::Train, 0).unwrap_err();
        assert!(err.contains("future-dated"), "unexpected error: {err}");
    }

    #[test]
    fn future_wallet_data_rejected_in_decision_path() {
        let config = base_config();
        let mut signal = make_signal(
            "2024-01-15T12:00:00Z",
            dec!(0.0001),
            dec!(100000),
            vec![(dec!(0.00011), dec!(100000))],
        );
        signal.wallets[0].updated_at = "2024-01-15T12:01:00Z".parse().unwrap();
        let err =
            simulate_signal(&signal, &config, &cost_assumptions(), Split::Train, 0).unwrap_err();
        assert!(
            err.contains("wallet evidence below configured confidence threshold"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn future_cost_data_rejected_in_decision_path() {
        let config = base_config();
        let mut signal = make_signal(
            "2024-01-15T12:00:00Z",
            dec!(0.0001),
            dec!(100000),
            vec![(dec!(0.00011), dec!(100000))],
        );
        signal.costs.observed_at = "2024-01-15T12:01:00Z".parse().unwrap();
        let err =
            simulate_signal(&signal, &config, &cost_assumptions(), Split::Train, 0).unwrap_err();
        assert!(
            err.contains("insufficient economic evidence"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn production_economic_edge_threshold_is_used() {
        // base_config uses the production min_expected_net_return_pct = 2.0.
        // The expected value is reconstructed from the point-in-time cost
        // model only, so the production threshold — not any backtest
        // override — decides acceptance here.
        let config = base_config();
        let signal = make_signal(
            "2024-01-15T12:00:00Z",
            dec!(0.0001),
            dec!(100000),
            vec![(dec!(0.00011), dec!(100000))],
        );
        assert!(simulate_signal(&signal, &config, &cost_assumptions(), Split::Train, 0).is_ok());

        // A stricter edge than production rejects the same signal.
        let mut strict = base_config();
        strict.economics.min_expected_net_return_pct = dec!(6.0);
        let err =
            simulate_signal(&signal, &strict, &cost_assumptions(), Split::Train, 0).unwrap_err();
        assert!(
            err.contains("economic edge below threshold"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn production_signal_score_threshold_is_used() {
        // base_config uses the production min_signal_score = 65.0.
        let config = base_config();
        let signal = make_signal(
            "2024-01-15T12:00:00Z",
            dec!(0.0001),
            dec!(100000),
            vec![(dec!(0.00011), dec!(100000))],
        );
        assert!(simulate_signal(&signal, &config, &cost_assumptions(), Split::Train, 0).is_ok());

        let mut strict = base_config();
        strict.strategy.min_signal_score = dec!(70.0);
        let err =
            simulate_signal(&signal, &strict, &cost_assumptions(), Split::Train, 0).unwrap_err();
        assert!(
            err.contains("signal confidence below threshold"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn production_round_trip_cost_gate_is_enforced() {
        // Mirrors the production authorize_entry gate: round-trip cost of the
        // point-in-time cost model must be within the configured threshold.
        let mut config = base_config();
        let signal = make_signal(
            "2024-01-15T12:00:00Z",
            dec!(0.0001),
            dec!(100000),
            vec![(dec!(0.00011), dec!(100000))],
        );
        // The fixture cost model's round-trip cost is 2.105% of position.
        config.economics.round_trip_cost_threshold_pct = dec!(2.0);
        let err =
            simulate_signal(&signal, &config, &cost_assumptions(), Split::Train, 0).unwrap_err();
        assert!(
            err.contains("exceeds") && err.contains("threshold"),
            "unexpected error: {err}"
        );

        config.economics.round_trip_cost_threshold_pct = dec!(3.0);
        assert!(simulate_signal(&signal, &config, &cost_assumptions(), Split::Train, 0).is_ok());
    }

    #[test]
    fn rejected_signal_returns_error() {
        let mut config = base_config();
        config.strategy.min_wallet_score = dec!(100);
        let signal = make_signal(
            "2024-01-15T12:00:00Z",
            dec!(0.0001),
            dec!(100000),
            vec![(dec!(0.00011), dec!(100000))],
        );
        let result = simulate_signal(&signal, &config, &cost_assumptions(), Split::Train, 0);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("wallet evidence"));
    }

    #[test]
    fn mfe_and_mae_computed_correctly() {
        let config = base_config();
        let signal = make_signal(
            "2024-01-15T12:00:00Z",
            dec!(0.0001),
            dec!(100000),
            vec![
                (dec!(0.00011), dec!(100000)),
                (dec!(0.000097), dec!(100000)),
            ],
        );
        let trade =
            simulate_signal(&signal, &config, &cost_assumptions(), Split::Train, 0).unwrap();
        assert_eq!(trade.mfe_pct, dec!(10)); // +10% from 0.0001 to 0.00011
        assert_eq!(trade.mae_pct, dec!(-3)); // -3% from 0.0001 to 0.000097
    }

    #[test]
    fn mfe_includes_exit_observation() {
        // The exit observation should be included in MFE/MAE.
        let config = base_config();
        let signal = make_signal(
            "2024-01-15T12:00:00Z",
            dec!(0.0001),
            dec!(100000),
            vec![
                (dec!(0.000105), dec!(100000)),
                (dec!(0.000112), dec!(100000)),
            ],
        );
        let trade =
            simulate_signal(&signal, &config, &cost_assumptions(), Split::Train, 0).unwrap();
        // TP at 12% triggers at obs 2 (0.000112). MFE should be 12%.
        assert_eq!(trade.exit_reason, ExitReason::TakeProfit);
        assert_eq!(trade.mfe_pct, dec!(12));
    }

    #[test]
    fn mfe_mae_exclude_observations_after_exit() {
        // TP fires at obs 2 (+12%); obs 3 (+100%) is after the actual exit
        // and must never contribute to MFE/MAE.
        let config = base_config();
        let signal = make_signal(
            "2024-01-15T12:00:00Z",
            dec!(0.0001),
            dec!(100000),
            vec![
                (dec!(0.000097), dec!(100000)), // -3%
                (dec!(0.000112), dec!(100000)), // +12% → TP here
                (dec!(0.0002), dec!(100000)),   // +100% — after exit, ignored
            ],
        );
        let trade =
            simulate_signal(&signal, &config, &cost_assumptions(), Split::Train, 0).unwrap();
        assert_eq!(trade.exit_reason, ExitReason::TakeProfit);
        assert_eq!(trade.mfe_pct, dec!(12));
        assert_eq!(trade.mae_pct, dec!(-3));
    }

    #[test]
    fn trailing_stop_uses_high_water_from_prior_observations() {
        // The trailing stop should use high water from PRIOR observations,
        // not the current candle.
        let config = base_config();
        let signal = make_signal(
            "2024-01-15T12:00:00Z",
            dec!(0.0001),
            dec!(100000),
            vec![
                (dec!(0.00011), dec!(100000)),
                (dec!(0.000100), dec!(100000)),
            ],
        );
        let trade =
            simulate_signal(&signal, &config, &cost_assumptions(), Split::Train, 0).unwrap();
        // High water = 0.00011 (from obs 1). Trailing stop at 4% from 0.00011 = 0.0001056.
        // Obs 2 price = 0.000100 < 0.0001056 → trailing stop triggers.
        assert_eq!(trade.exit_reason, ExitReason::TrailingStop);
    }

    #[test]
    fn trailing_high_water_state_persists_across_observations() {
        // State ordering: obs 1 raises the high-water (no exit on obs 1
        // itself); obs 2 stays above the trailing threshold set by obs 1's
        // high (no exit — high-water unchanged); obs 3 falls below that
        // threshold and exits there. The threshold tracked from obs 1 must
        // persist, and obs 2's lower close must not lower it.
        let config = base_config();
        let signal = make_signal(
            "2024-01-15T12:00:00Z",
            dec!(0.0001),
            dec!(100000),
            vec![
                (dec!(0.00011), dec!(100000)),  // new high-water 0.00011
                (dec!(0.000106), dec!(100000)), // > 0.0001056 → no exit
                (dec!(0.000105), dec!(100000)), // <= 0.0001056 → TrailingStop
            ],
        );
        let trade =
            simulate_signal(&signal, &config, &cost_assumptions(), Split::Train, 0).unwrap();
        assert_eq!(trade.exit_reason, ExitReason::TrailingStop);
        assert_eq!(trade.holding_minutes, 15);
        assert_eq!(trade.mfe_pct, dec!(10));
    }

    #[test]
    fn stop_loss_level_always_exits_at_own_observation() {
        // Backstop for the conservative ambiguity rule: with close-price
        // observations, an observation at/below the stop level exits at its
        // own evaluation, so the walk can never proceed past a stop into a
        // later favorable (TP-side) price. A stop-level close followed by a
        // far-above-TP close must exit at the stop, not book the TP.
        let config = base_config();
        let signal = make_signal(
            "2024-01-15T12:00:00Z",
            dec!(0.0001),
            dec!(100000),
            vec![
                (dec!(0.000095), dec!(100000)), // exactly at stop (-5%) → exit here
                (dec!(0.00015), dec!(100000)),  // +50%, beyond TP — never reached
            ],
        );
        let trade =
            simulate_signal(&signal, &config, &cost_assumptions(), Split::Train, 0).unwrap();
        assert_eq!(trade.exit_reason, ExitReason::StopLoss);
        assert_eq!(trade.exit_price_usd, dec!(0.000095));
        assert!(!trade.is_ambiguous);
        assert_eq!(trade.mfe_pct, Decimal::ZERO);
    }

    #[test]
    fn ambiguity_at_exact_thresholds() {
        // Boundary of the documented conservative rule: an interval whose
        // observed range exactly touches both the stop and the target level
        // is ambiguous; one tick inside on either side is not.
        let entry = dec!(0.0001);
        let sl = entry * (dec!(1) - dec!(5) / dec!(100));
        let tp = entry * (dec!(1) + dec!(12) / dec!(100));
        assert!(detect_ambiguity(sl, tp, entry, dec!(5), dec!(12)).is_some());
        assert!(detect_ambiguity(sl + dec!(0.0000001), tp, entry, dec!(5), dec!(12)).is_none());
        assert!(detect_ambiguity(sl, tp - dec!(0.0000001), entry, dec!(5), dec!(12)).is_none());
    }

    #[test]
    fn ambiguity_detected_when_sl_tp_both_crossed() {
        let prev_price = dec!(0.000094); // -6% from entry (below SL at -5%)
        let curr_price = dec!(0.000115); // +15% from entry (above TP at +12%)
        let entry_price = dec!(0.0001);
        let result = detect_ambiguity(prev_price, curr_price, entry_price, dec!(5), dec!(12));
        assert!(result.is_some());
    }

    #[test]
    fn no_ambiguity_when_only_sl_crossed() {
        let prev_price = dec!(0.0001);
        let curr_price = dec!(0.000094);
        let entry_price = dec!(0.0001);
        let result = detect_ambiguity(prev_price, curr_price, entry_price, dec!(5), dec!(12));
        assert!(result.is_none());
    }

    #[test]
    fn no_ambiguity_when_only_tp_crossed() {
        let prev_price = dec!(0.0001);
        let curr_price = dec!(0.000115);
        let entry_price = dec!(0.0001);
        let result = detect_ambiguity(prev_price, curr_price, entry_price, dec!(5), dec!(12));
        assert!(result.is_none());
    }

    #[test]
    fn deterministic_ids_are_stable() {
        let config = base_config();
        let signal = make_signal(
            "2024-01-15T12:00:00Z",
            dec!(0.0001),
            dec!(100000),
            vec![(dec!(0.00011), dec!(100000))],
        );
        let t1 = simulate_signal(&signal, &config, &cost_assumptions(), Split::Train, 0).unwrap();
        let t2 = simulate_signal(&signal, &config, &cost_assumptions(), Split::Train, 0).unwrap();
        // Same input → same deterministic ID
        assert_eq!(t1.trade_id, t2.trade_id);
    }

    #[test]
    fn different_index_produces_different_id() {
        let config = base_config();
        let signal = make_signal(
            "2024-01-15T12:00:00Z",
            dec!(0.0001),
            dec!(100000),
            vec![(dec!(0.00011), dec!(100000))],
        );
        let t1 = simulate_signal(&signal, &config, &cost_assumptions(), Split::Train, 0).unwrap();
        let t2 = simulate_signal(&signal, &config, &cost_assumptions(), Split::Train, 1).unwrap();
        assert_ne!(t1.trade_id, t2.trade_id);
    }

    #[test]
    fn entry_quantity_not_reduced_by_slippage() {
        // Verify that entry quantity = position_usd / entry_price exactly,
        // with no slippage_factor reduction (slippage is in costs only).
        let config = base_config();
        let signal = make_signal(
            "2024-01-15T12:00:00Z",
            dec!(0.0001),
            dec!(100000),
            vec![(dec!(0.00011), dec!(100000))],
        );
        let trade =
            simulate_signal(&signal, &config, &cost_assumptions(), Split::Train, 0).unwrap();
        let expected_quantity = signal.position_usd / dec!(0.0001);
        assert_eq!(trade.entry_quantity_tokens, expected_quantity);
    }

    #[test]
    fn failed_tx_cost_is_probabilistic_not_fixed() {
        let ca = cost_assumptions();
        // failed_tx_rate = 0.05, failed_tx_cost_usd = 0.002
        // Expected cost = 2 * 0.05 * 0.002 = 0.0002
        let expected = dec!(2) * dec!(0.05) * dec!(0.002);
        assert_eq!(ca.expected_failed_tx_cost(), expected);
    }
}
