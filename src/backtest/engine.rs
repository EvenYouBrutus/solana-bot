use crate::backtest::data::HistoricalSignal;
use crate::backtest::split::Split;
use crate::config::types::Config;
use crate::domain::market::MarketSnapshot;
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
use uuid::Uuid;

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
    /// Probability of a failed transaction attempt.
    pub failed_tx_rate: Decimal,
    /// Cost per failed transaction in USD.
    pub failed_tx_cost_usd: Decimal,
}

impl CostAssumptions {
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

    pub fn failed_tx_cost(&self) -> Decimal {
        dec!(2) * self.failed_tx_rate * self.failed_tx_cost_usd
    }
}

/// Detect ambiguous OHLC situations where both SL and TP thresholds are
/// crossed between consecutive observations and ordering cannot be determined.
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
        position_id: Some(Uuid::new_v4().to_string()),
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
        entry_signature: format!("backtest:{}", Uuid::new_v4()),
        high_water_price_usd: entry_price,
        realized_pnl_usd: Decimal::ZERO,
        unrealized_pnl_usd: Decimal::ZERO,
        fees_usd: Decimal::ZERO,
        current_value_usd: signal.position_usd,
        signal_id: Uuid::new_v4().to_string(),
        exit_reason: None,
    }
}

/// Simulate a single historical signal through the full entry/exit pipeline.
///
/// Returns the complete trade record, or a rejection reason if the signal
/// would not have been accepted.
pub fn simulate_signal(
    signal: &HistoricalSignal,
    config: &Config,
    cost_assumptions: &CostAssumptions,
    split: Split,
) -> Result<SimulatedTrade, String> {
    // --- Point-in-time entry decision ---
    // Build ExpectedValue from the signal's own cost model and expected return.
    let expected = ExpectedValue::estimate(
        signal.expected_gross_return_pct,
        &signal.costs,
        dec!(0),
        dec!(0),
        config.economics.uncertainty_haircut_pct,
    )
    .map_err(|e| format!("economic estimation failed: {e}"))?;

    let wallet_refs: Vec<&WalletStats> = signal.wallets.iter().collect();

    // Use evaluate_signal with a point-in-time adaptation: set the config's
    // max_data_age_secs very large and ensure all timestamps are relative
    // to signal_timestamp. The evaluate_signal function uses Utc::now() for
    // freshness checks, so we adapt by ensuring market.observed_at == signal_timestamp
    // and making max_data_age_secs large enough.
    //
    // However, evaluate_signal uses `Utc::now()` directly. For true PIT
    // correctness, we use our own PIT evaluation that faithfully replicates
    // the same logic but with an explicit `now` parameter.
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
    let slippage_factor = (dec!(10000) - cost_assumptions.entry_slippage_bps) / dec!(10000);
    let entry_quantity_tokens = (signal.position_usd / entry_price) * slippage_factor;
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

        // Update high water mark
        if price > high_water {
            high_water = price;
            position.high_water_price_usd = price;
        }

        // Check for OHLC ambiguity: both SL and TP in the price range
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

        // Compute return for MFE/MAE
        let return_pct = (price - entry_price) / entry_price * dec!(100);
        if return_pct > mfe {
            mfe = return_pct;
        }
        if return_pct < mae {
            mae = return_pct;
        }

        // Check exit conditions using existing exit_reason logic
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
    }

    // If no exit triggered, close at last observation (time limit / end of data)
    if exit_reason_found.is_none() {
        if let Some(last_obs) = signal.price_history.last() {
            exit_price = last_obs.price_usd;
            exit_time = last_obs.timestamp;
            exit_reason_found = Some(ExitReason::TimeLimit);
        }
    }

    let exit_reason = exit_reason_found.unwrap_or(ExitReason::TimeLimit);
    let holding_minutes = (exit_time - entry_time).num_minutes();

    // --- Exit cost simulation ---
    let exit_proceeds_gross = entry_quantity_tokens * exit_price;
    let exit_costs = cost_assumptions.exit_costs(exit_proceeds_gross);
    let failed_tx = cost_assumptions.failed_tx_cost();
    let total_cost = entry_costs.total_usd + exit_costs.total_usd + failed_tx;

    // --- PnL ---
    let gross_pnl = exit_proceeds_gross - signal.position_usd;
    let net_pnl = gross_pnl - total_cost;
    let gross_return_pct = gross_pnl / signal.position_usd * dec!(100);
    let net_return_pct = net_pnl / signal.position_usd * dec!(100);

    Ok(SimulatedTrade {
        trade_id: Uuid::new_v4().to_string(),
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
        exit_reason,
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
    })
}

/// Point-in-time version of evaluate_signal that accepts an explicit `now`
/// parameter instead of using `Utc::now()`. Faithfully reproduces the same
/// validation order and thresholds from the production `evaluate_signal`.
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
    // Economic edge
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
        id: Uuid::new_v4().to_string(),
        mint: mint.into(),
        wallets: wallets.iter().map(|w| w.wallet.clone()).collect(),
        side: Side::Buy,
        score,
        expected_gross_return_pct: expected.gross_return_pct,
        created_at: now,
        reason: "qualified-wallet accumulation with liquid safe market".into(),
    }))
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
min_signal_score = 65.0
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
        let trade = simulate_signal(&signal, &config, &cost_assumptions(), Split::Train).unwrap();
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
        let trade = simulate_signal(&signal, &config, &cost_assumptions(), Split::Train).unwrap();
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
        let trade = simulate_signal(&signal, &config, &cost_assumptions(), Split::Train).unwrap();
        // Price went up 10% then fell back; trailing stop at 4% from high
        assert!(
            trade.exit_reason == ExitReason::TrailingStop
                || trade.exit_reason == ExitReason::TakeProfit
        );
    }

    #[test]
    fn time_limit_exit() {
        let config = base_config();
        let signal = make_signal(
            "2024-01-15T12:00:00Z",
            dec!(0.0001),
            dec!(100000),
            vec![
                (dec!(0.000101), dec!(100000)),
                (dec!(0.000102), dec!(100000)),
            ],
        );
        let trade = simulate_signal(&signal, &config, &cost_assumptions(), Split::Train).unwrap();
        // Small moves that don't hit SL/TP, exit at end of data
        assert_eq!(trade.exit_reason, ExitReason::TimeLimit);
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
        let trade = simulate_signal(&signal, &config, &cost_assumptions(), Split::Train).unwrap();
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
        let trade = simulate_signal(&signal, &config, &cost_assumptions(), Split::Train).unwrap();
        assert!(!trade.entry_costs.is_observed);
        assert!(!trade.exit_costs.is_observed);
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
        let trade = simulate_signal(&signal, &config, &ca, Split::Train).unwrap();
        let expected_total_cost = ca.entry_costs(signal.position_usd).total_usd
            + ca.exit_costs(trade.gross_pnl_usd + signal.position_usd)
                .total_usd
            + ca.failed_tx_cost();
        assert_eq!(trade.total_cost_usd, expected_total_cost);
        assert_eq!(
            trade.net_pnl_usd,
            trade.gross_pnl_usd - trade.total_cost_usd
        );
    }

    #[test]
    fn expected_gross_return_pct_not_used_as_realized() {
        let config = base_config();
        let mut signal = make_signal(
            "2024-01-15T12:00:00Z",
            dec!(0.0001),
            dec!(100000),
            vec![(dec!(0.00011), dec!(100000))],
        );
        signal.expected_gross_return_pct = dec!(999);
        let trade = simulate_signal(&signal, &config, &cost_assumptions(), Split::Train).unwrap();
        // Realized return must reflect actual price path, never expected_gross_return_pct
        assert_ne!(trade.gross_return_pct, dec!(999));
        assert!(trade.gross_return_pct > Decimal::ZERO);
        assert!(trade.gross_return_pct < dec!(100));
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
        let result = simulate_signal(&signal, &config, &cost_assumptions(), Split::Train);
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
        let trade = simulate_signal(&signal, &config, &cost_assumptions(), Split::Train).unwrap();
        assert_eq!(trade.mfe_pct, dec!(10)); // +10% from 0.0001 to 0.00011
        assert_eq!(trade.mae_pct, dec!(-3)); // -3% from 0.0001 to 0.000097
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
}
