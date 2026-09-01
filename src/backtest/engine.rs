use crate::backtest::data::HistoricalSignal;
use crate::backtest::split::Split;
use crate::config::types::Config;
use crate::domain::position::{Position, PositionState, ReconciliationStatus};
use crate::domain::signal::{SignalScore, TradeSignal};
use crate::domain::token::TokenSafety;
use crate::domain::wallet::{Side, WalletStats};
use crate::economics::ExpectedValue;
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
    /// Compute entry costs for a given notional USD amount.
    ///
    /// These are ALL MODELED assumptions. Slippage is captured as a separate
    /// dollar cost here; it is NOT also applied as a quantity reduction.
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
    /// These are ALL MODELED assumptions.
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
/// - The economic gate uses only point-in-time cost data.
/// - Entry quantity is computed at market price; execution effects (slippage,
///   impact, fees) are captured as modeled dollar costs, not quantity reductions.
/// - MFE/MAE are computed from all observations up to and including the exit.
/// - The trailing stop evaluates against high-water from prior observations
///   (the current observation's price is evaluated against the previous high).
/// - IDs are deterministic (no randomness).
pub fn simulate_signal(
    signal: &HistoricalSignal,
    config: &Config,
    cost_assumptions: &CostAssumptions,
    split: Split,
    trade_index: usize,
    min_edge_override: Option<Decimal>,
    min_signal_score_override: Option<Decimal>,
) -> Result<SimulatedTrade, String> {
    // --- Price path sufficiency check ---
    // The data must span at least max_holding_minutes from entry to permit
    // a valid TimeLimit exit. If not, and no exit trigger occurs, the trade
    // must be marked Censored.
    let max_hold = chrono::Duration::minutes(config.strategy.max_holding_minutes);
    let price_path_sufficient = if let Some(last_obs) = signal.price_history.last() {
        (last_obs.timestamp - signal.signal_timestamp) >= max_hold
    } else {
        false
    };

    // --- Point-in-time entry decision ---
    // Compute break-even return from the signal's cost model. This is the
    // minimum gross return needed to cover all modeled costs. The signal's
    // own expected_gross_return_pct is NEVER used.
    let cost_result = signal
        .costs
        .calculate()
        .map_err(|e| format!("cost model calculation failed: {e}"))?;
    let break_even_gross_return =
        cost_result.round_trip_cost_pct_of_position + config.economics.uncertainty_haircut_pct;

    let expected = ExpectedValue::estimate(
        break_even_gross_return,
        &signal.costs,
        dec!(0),
        dec!(0),
        config.economics.uncertainty_haircut_pct,
    )
    .map_err(|e| format!("economic estimation failed: {e}"))?;

    let wallet_refs: Vec<&WalletStats> = signal.wallets.iter().collect();

    let decision = evaluate_signal_pit(
        config,
        &signal.mint,
        &wallet_refs,
        &signal.market,
        &signal.safety,
        &expected,
        signal.signal_timestamp,
        min_edge_override,
        min_signal_score_override,
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
                exit_price = price;
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
    let (exit_reason_final, is_censored, censored_reason) = match &exit_reason_found {
        Some(reason) => (reason.clone(), false, None),
        None => {
            // No exit trigger occurred. Determine if this is a valid TimeLimit
            // or if the data is insufficient (Censored).
            if price_path_sufficient {
                // Data spans >= max_holding_minutes: valid TimeLimit exit.
                if signal.price_history.last().is_some() {
                    (ExitReason::TimeLimit, false, None)
                } else {
                    (
                        ExitReason::Censored,
                        true,
                        Some("no price observations".into()),
                    )
                }
            } else {
                // Data spans < max_holding_minutes: insufficient data.
                // Mark as Censored, not TimeLimit.
                (
                    ExitReason::Censored,
                    true,
                    Some(format!(
                        "price history spans {} minutes but max_holding is {} minutes; \
                         insufficient future data to determine exit",
                        signal
                            .price_history
                            .last()
                            .map(|o| (o.timestamp - signal.signal_timestamp).num_minutes())
                            .unwrap_or(0),
                        config.strategy.max_holding_minutes
                    )),
                )
            }
        }
    };

    // Set exit_price and exit_time for the no-trigger case.
    if exit_reason_found.is_none() {
        if let Some(last_obs) = signal.price_history.last() {
            exit_price = last_obs.price_usd;
            exit_time = last_obs.timestamp;
        }
    }

    let holding_minutes = (exit_time - entry_time).num_minutes();

    // --- Exit cost simulation ---
    // Exit proceeds: tokens * exit_price. Slippage is in exit_costs, not
    // a quantity reduction, consistent with entry treatment.
    let exit_proceeds_gross = entry_quantity_tokens * exit_price;
    let exit_costs = cost_assumptions.exit_costs(exit_proceeds_gross);
    let failed_tx = cost_assumptions.expected_failed_tx_cost();
    let total_cost = entry_costs.total_usd + exit_costs.total_usd + failed_tx;

    // --- PnL ---
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
        cost_mode: CostMode::Modeled,
    })
}

/// Point-in-time version of evaluate_signal that accepts an explicit `now`
/// parameter instead of using `Utc::now()`. Faithfully reproduces the same
/// validation order and thresholds from the production `evaluate_signal`.
#[allow(clippy::too_many_arguments)]
fn evaluate_signal_pit(
    config: &Config,
    mint: &str,
    wallets: &[&WalletStats],
    market: &MarketSnapshot,
    safety: &TokenSafety,
    expected: &ExpectedValue,
    now: DateTime<Utc>,
    min_edge_override: Option<Decimal>,
    min_signal_score_override: Option<Decimal>,
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
    // Economic edge — uses break-even return from cost model, NOT
    // signal.expected_gross_return_pct. This is strictly PIT.
    let min_edge = min_edge_override.unwrap_or(config.economics.min_expected_net_return_pct);
    if expected.net_return_pct < min_edge {
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
    let min_score = min_signal_score_override.unwrap_or(config.strategy.min_signal_score);
    if score.final_signal_score < min_score {
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
#[derive(Debug, Clone, Deserialize)]
pub struct BacktestConfig {
    pub split: crate::backtest::split::SplitConfig,
    #[serde(default)]
    pub costs: BacktestCostConfig,
    /// Starting capital in USD for drawdown calculations.
    #[serde(default = "default_capital")]
    pub capital_usd: rust_decimal::Decimal,
    /// Override for min_expected_net_return_pct (bypasses config validation).
    pub min_expected_net_return_pct: Option<rust_decimal::Decimal>,
    /// Override for min_signal_score.
    pub min_signal_score: Option<rust_decimal::Decimal>,
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
        let text = r#"
mode = "paper"
[rpc]
http_endpoints = ["https://api.test"]
max_data_age_secs = 999999
[strategy]
base_mint = "So11111111111111111111111111111111111111112"
min_wallet_score = 60.0
min_wallet_samples = 25
min_consensus_wallets = 2
            min_signal_score = 50.0
min_token_age_secs = 86400
stop_loss_pct = 5.0
take_profit_pct = 12.0
trailing_stop_pct = 4.0
max_holding_minutes = 240
[economics]
round_trip_cost_threshold_pct = 100.0
min_expected_net_return_pct = -100.0
max_quote_age_secs = 999999
uncertainty_haircut_pct = 0
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
min_liquidity_usd = 1.0
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
            volatility_pct: dec!(15),
            buy_sell_imbalance: dec!(0.6),
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
        let trade = simulate_signal(
            &signal,
            &config,
            &cost_assumptions(),
            Split::Train,
            0,
            None,
            None,
        )
        .unwrap();
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
        let trade = simulate_signal(
            &signal,
            &config,
            &cost_assumptions(),
            Split::Train,
            0,
            None,
            None,
        )
        .unwrap();
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
        let trade = simulate_signal(
            &signal,
            &config,
            &cost_assumptions(),
            Split::Train,
            0,
            None,
            None,
        )
        .unwrap();
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
        let trade = simulate_signal(
            &signal,
            &config,
            &cost_assumptions(),
            Split::Train,
            0,
            None,
            None,
        )
        .unwrap();
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
        let trade = simulate_signal(
            &signal,
            &config,
            &cost_assumptions(),
            Split::Train,
            0,
            None,
            None,
        )
        .unwrap();
        assert_eq!(trade.exit_reason, ExitReason::Censored);
        assert!(trade.is_censored);
        assert!(trade.censored_reason.is_some());
        assert!(trade
            .censored_reason
            .unwrap()
            .contains("insufficient future data"));
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
        let trade = simulate_signal(
            &signal,
            &config,
            &cost_assumptions(),
            Split::Train,
            0,
            None,
            None,
        )
        .unwrap();
        assert_eq!(trade.exit_reason, ExitReason::Censored);
        assert!(trade.is_censored);
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
        let trade = simulate_signal(
            &signal,
            &config,
            &cost_assumptions(),
            Split::Train,
            0,
            None,
            None,
        )
        .unwrap();
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
        let trade = simulate_signal(
            &signal,
            &config,
            &cost_assumptions(),
            Split::Train,
            0,
            None,
            None,
        )
        .unwrap();
        assert!(!trade.entry_costs.is_observed);
        assert!(!trade.exit_costs.is_observed);
        assert_eq!(trade.cost_mode, CostMode::Modeled);
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
        let trade = simulate_signal(&signal, &config, &ca, Split::Train, 0, None, None).unwrap();
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

        let result_low = simulate_signal(
            &signal_low,
            &config,
            &cost_assumptions(),
            Split::Train,
            0,
            None,
            None,
        );
        let result_high = simulate_signal(
            &signal_high,
            &config,
            &cost_assumptions(),
            Split::Train,
            0,
            None,
            None,
        );

        // Both must produce the same outcome: same exit reason, same PnL.
        assert_eq!(result_low.is_ok(), result_high.is_ok());
        if let (Ok(tl), Ok(th)) = (&result_low, &result_high) {
            assert_eq!(tl.exit_reason, th.exit_reason);
            assert_eq!(tl.gross_pnl_usd, th.gross_pnl_usd);
            assert_eq!(tl.net_pnl_usd, th.net_pnl_usd);
        }
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
        let result = simulate_signal(
            &signal,
            &config,
            &cost_assumptions(),
            Split::Train,
            0,
            None,
            None,
        );
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
        let trade = simulate_signal(
            &signal,
            &config,
            &cost_assumptions(),
            Split::Train,
            0,
            None,
            None,
        )
        .unwrap();
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
        let trade = simulate_signal(
            &signal,
            &config,
            &cost_assumptions(),
            Split::Train,
            0,
            None,
            None,
        )
        .unwrap();
        // TP at 12% triggers at obs 2 (0.000112). MFE should be 12%.
        assert_eq!(trade.exit_reason, ExitReason::TakeProfit);
        assert_eq!(trade.mfe_pct, dec!(12));
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
        let trade = simulate_signal(
            &signal,
            &config,
            &cost_assumptions(),
            Split::Train,
            0,
            None,
            None,
        )
        .unwrap();
        // High water = 0.00011 (from obs 1). Trailing stop at 4% from 0.00011 = 0.0001056.
        // Obs 2 price = 0.000100 < 0.0001056 → trailing stop triggers.
        assert_eq!(trade.exit_reason, ExitReason::TrailingStop);
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
        let t1 = simulate_signal(
            &signal,
            &config,
            &cost_assumptions(),
            Split::Train,
            0,
            None,
            None,
        )
        .unwrap();
        let t2 = simulate_signal(
            &signal,
            &config,
            &cost_assumptions(),
            Split::Train,
            0,
            None,
            None,
        )
        .unwrap();
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
        let t1 = simulate_signal(
            &signal,
            &config,
            &cost_assumptions(),
            Split::Train,
            0,
            None,
            None,
        )
        .unwrap();
        let t2 = simulate_signal(
            &signal,
            &config,
            &cost_assumptions(),
            Split::Train,
            1,
            None,
            None,
        )
        .unwrap();
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
        let trade = simulate_signal(
            &signal,
            &config,
            &cost_assumptions(),
            Split::Train,
            0,
            None,
            None,
        )
        .unwrap();
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
