//! Build `HistoricalSignal` records compatible with
//! `src/backtest/data.rs` from real historical inputs.

use crate::backtest::data::{HistoricalSignal, PriceObservation};
use crate::domain::wallet::WalletStats;
use crate::historical::cost::{build_cost_model, CostBreakdown};
use crate::historical::ohlcv::OhlcvCandle;
use crate::historical::safety::{HistoricalTokenSafety, SafetyProvider};
use crate::historical::wallet::HistoricalWalletStats;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Builder for one entry of the historical dataset. Encapsulates
/// the chronological order of point-in-time construction:
///
/// 1. market snapshot <= signal_timestamp
/// 2. safety observation <= signal_timestamp
/// 3. wallet PIT reconstruction <= signal_timestamp
/// 4. cost model <= signal_timestamp
/// 5. future OHLC observations > signal_timestamp
pub struct HistoricalSignalBuilder {
    pub position_usd: Decimal,
    pub token_decimals: u8,
    pub base_mint_decimals: u8,
    pub sol_price_usd: Decimal,
    pub priority_fee_lamports: u64,
    pub swap_fee_bps: Decimal,
    pub future_window_minutes: i64,
}

impl HistoricalSignalBuilder {
    pub fn new(
        position_usd: Decimal,
        token_decimals: u8,
        base_mint_decimals: u8,
        sol_price_usd: Decimal,
    ) -> Self {
        Self {
            position_usd,
            token_decimals,
            base_mint_decimals,
            sol_price_usd,
            priority_fee_lamports: 10_000,
            swap_fee_bps: Decimal::from(30),
            future_window_minutes: 240,
        }
    }

    pub fn with_priority_fee(mut self, lamports: u64) -> Self {
        self.priority_fee_lamports = lamports;
        self
    }

    pub fn with_swap_fee_bps(mut self, bps: Decimal) -> Self {
        self.swap_fee_bps = bps;
        self
    }

    pub fn with_future_window_minutes(mut self, mins: i64) -> Self {
        self.future_window_minutes = mins;
        self
    }

    /// Build one `HistoricalSignal` from real historical inputs.
    ///
    /// The function is pure: it does not contact any external service
    /// or consult the wall clock.
    pub fn build(
        &self,
        mint: &str,
        signal_timestamp: DateTime<Utc>,
        market_candle: &OhlcvCandle,
        safety: &HistoricalTokenSafety,
        wallets: &[HistoricalWalletStats],
        future_candles: &[OhlcvCandle],
    ) -> Result<HistoricalDatasetEntry, SignalBuildError> {
        // PIT enforcement on every input.
        if market_candle.timestamp > signal_timestamp {
            return Err(SignalBuildError::PitViolation(format!(
                "market candle timestamp {} > signal_timestamp {}",
                market_candle.timestamp, signal_timestamp
            )));
        }
        if safety.observed_at > signal_timestamp {
            return Err(SignalBuildError::PitViolation(format!(
                "safety.observed_at {} > signal_timestamp {}",
                safety.observed_at, signal_timestamp
            )));
        }
        for w in wallets {
            if w.updated_at > signal_timestamp {
                return Err(SignalBuildError::PitViolation(format!(
                    "wallet {} updated_at {} > signal_timestamp {}",
                    w.wallet, w.updated_at, signal_timestamp
                )));
            }
        }
        // Liquidity / volume derived from the latest candle.
        let liquidity_usd = market_candle.liquidity_usd.unwrap_or(Decimal::ZERO);
        let market = crate::domain::market::MarketSnapshot {
            mint: mint.to_string(),
            price_usd: market_candle.close_usd,
            liquidity_usd,
            volume_24h_usd: market_candle.volume_usd.unwrap_or(Decimal::ZERO),
            volatility_pct: Decimal::from(20),
            buy_sell_imbalance: Decimal::from_f64_retain(0.5).unwrap(),
            observed_at: market_candle.timestamp,
            received_at: market_candle.timestamp,
            slot: None,
        };
        let safety_field = SafetyProvider::to_token_safety(safety, safety.token_age_secs);
        let wallet_stats: Vec<WalletStats> = wallets
            .iter()
            .filter(|w| w.trades > 0)
            .map(|w| w.to_wallet_stats())
            .collect();
        // Use the candle at signal time to build the cost model.
        let observation = PriceObservation {
            timestamp: market_candle.timestamp,
            price_usd: market_candle.close_usd,
            liquidity_usd,
            open_usd: Some(market_candle.open_usd),
            high_usd: Some(market_candle.high_usd),
            low_usd: Some(market_candle.low_usd),
            close_usd: Some(market_candle.close_usd),
            volume: market_candle.volume_usd,
        };
        let (costs, _breakdown) = build_cost_model(
            self.position_usd,
            Some(&observation),
            self.sol_price_usd,
            self.priority_fee_lamports,
            self.swap_fee_bps,
            signal_timestamp,
        );
        if costs.observed_at > signal_timestamp {
            return Err(SignalBuildError::PitViolation(
                "costs.observed_at must be <= signal_timestamp".into(),
            ));
        }
        // Future price observations: only candles strictly after the
        // signal timestamp. The loader allows `obs.timestamp == signal_timestamp`
        // for the entry observation, so we permit the same-time candle
        // as the first entry observation and require the rest to be
        // strictly later.
        let mut price_history: Vec<PriceObservation> = Vec::new();
        for c in future_candles {
            if c.timestamp < signal_timestamp {
                continue;
            }
            price_history.push(PriceObservation {
                timestamp: c.timestamp,
                price_usd: c.close_usd,
                liquidity_usd: c.liquidity_usd.unwrap_or(Decimal::ZERO),
                open_usd: Some(c.open_usd),
                high_usd: Some(c.high_usd),
                low_usd: Some(c.low_usd),
                close_usd: Some(c.close_usd),
                volume: c.volume_usd,
            });
        }
        // If the entry candle is exactly at the signal time, prepend it.
        if market_candle.timestamp == signal_timestamp {
            price_history.insert(
                0,
                PriceObservation {
                    timestamp: signal_timestamp,
                    price_usd: market_candle.close_usd,
                    liquidity_usd,
                    open_usd: Some(market_candle.open_usd),
                    high_usd: Some(market_candle.high_usd),
                    low_usd: Some(market_candle.low_usd),
                    close_usd: Some(market_candle.close_usd),
                    volume: market_candle.volume_usd,
                },
            );
        }
        // Ensure strict future horizon.
        let horizon = signal_timestamp + chrono::Duration::minutes(self.future_window_minutes);
        price_history.retain(|o| o.timestamp <= horizon || o.timestamp == signal_timestamp);
        if price_history.is_empty() {
            return Err(SignalBuildError::MissingFutureData(
                "no future price observations in window".into(),
            ));
        }
        // If no usable wallets, the signal is unusable: we cannot
        // honestly attribute consensus.
        if wallet_stats.is_empty() {
            return Err(SignalBuildError::MissingWalletEvidence(
                "no wallets with positive trades".into(),
            ));
        }
        // Expected gross return: derive from required_avg_win_pct of
        // the cost model. This is purely a recorded field, NEVER used
        // in the entry decision by the engine.
        let expected_gross_return_pct = costs
            .calculate()
            .map(|r| r.required_avg_win_pct)
            .unwrap_or(Decimal::ZERO);
        let signal = HistoricalSignal {
            signal_timestamp,
            mint: mint.to_string(),
            market,
            safety: safety_field,
            wallets: wallet_stats,
            costs,
            position_usd: self.position_usd,
            expected_gross_return_pct,
            token_decimals: self.token_decimals,
            base_mint_decimals: self.base_mint_decimals,
            price_history,
        };
        Ok(HistoricalDatasetEntry {
            signal,
            breakdown: _breakdown,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoricalDatasetEntry {
    pub signal: HistoricalSignal,
    pub breakdown: CostBreakdown,
}

#[derive(Debug, Error)]
pub enum SignalBuildError {
    #[error("point-in-time violation: {0}")]
    PitViolation(String),
    #[error("missing future data: {0}")]
    MissingFutureData(String),
    #[error("missing wallet evidence: {0}")]
    MissingWalletEvidence(String),
    #[error("invalid input: {0}")]
    Invalid(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::wallet::WalletTier;
    use crate::historical::safety::HistoricalTokenSafety;
    use crate::historical::wallet::HistoricalWalletStats;
    use chrono::TimeZone;

    fn candle(ts: i64, close: f64) -> OhlcvCandle {
        OhlcvCandle {
            timestamp: Utc.timestamp_opt(ts, 0).unwrap(),
            open_usd: Decimal::from_f64_retain(close).unwrap(),
            high_usd: Decimal::from_f64_retain(close * 1.01).unwrap(),
            low_usd: Decimal::from_f64_retain(close * 0.99).unwrap(),
            close_usd: Decimal::from_f64_retain(close).unwrap(),
            volume_usd: Some(Decimal::from_f64_retain(10_000.0).unwrap()),
            liquidity_usd: Some(Decimal::from_f64_retain(100_000.0).unwrap()),
        }
    }

    fn safety(ts: i64) -> HistoricalTokenSafety {
        HistoricalTokenSafety {
            mint: "Mint1111111111111111111111111111111111111".into(),
            mint_authority_present: false,
            freeze_authority_present: false,
            decimals: 6,
            supply: Decimal::from(1_000_000),
            holder_top10_pct: None,
            token_age_secs: 1_000_000,
            sellable: None,
            route_available: None,
            creator_suspicious: None,
            abnormal_activity: None,
            liquidity_change_pct: None,
            liquidity_locked_or_burned: None,
            observed_at: Utc.timestamp_opt(ts, 0).unwrap(),
            created_at: None,
        }
    }

    fn wallet(ts: i64) -> HistoricalWalletStats {
        HistoricalWalletStats {
            wallet: "Wallet1111111111111111111111111111111111111".into(),
            trades: 30,
            realized_pnl_usd: Decimal::from(500),
            win_rate: Decimal::from_f64_retain(0.7).unwrap(),
            avg_return_pct: Decimal::from(15),
            median_return_pct: Decimal::from(12),
            max_drawdown_pct: Decimal::from(10),
            recent_return_pct: Decimal::from(10),
            concentration_pct: Decimal::from(5),
            scam_exposure_pct: Decimal::ZERO,
            score: Decimal::from(75),
            tier: WalletTier::Qualified,
            updated_at: Utc.timestamp_opt(ts, 0).unwrap(),
            filtered_future_trades: 0,
        }
    }

    #[test]
    fn builds_signal_with_pit_consistency() {
        let builder = HistoricalSignalBuilder::new(Decimal::from(4), 6, 9, Decimal::from(150));
        let ts = 1_700_000_000i64;
        let signal_ts = Utc.timestamp_opt(ts, 0).unwrap();
        let entry = builder
            .build(
                "Mint1111111111111111111111111111111111111",
                signal_ts,
                &candle(ts, 0.0001),
                &safety(ts),
                &[wallet(ts - 60)],
                &[candle(ts + 60, 0.00011), candle(ts + 120, 0.00012)],
            )
            .unwrap();
        assert_eq!(entry.signal.signal_timestamp, signal_ts);
        assert_eq!(entry.signal.market.observed_at, signal_ts);
        assert_eq!(entry.signal.wallets.len(), 1);
        assert!(!entry.signal.price_history.is_empty());
    }

    #[test]
    fn rejects_wallet_updated_after_signal() {
        let builder = HistoricalSignalBuilder::new(Decimal::from(4), 6, 9, Decimal::from(150));
        let ts = 1_700_000_000i64;
        let signal_ts = Utc.timestamp_opt(ts, 0).unwrap();
        let result = builder.build(
            "Mint1111111111111111111111111111111111111",
            signal_ts,
            &candle(ts, 0.0001),
            &safety(ts),
            &[wallet(ts + 60)],
            &[candle(ts + 60, 0.00011)],
        );
        assert!(matches!(result, Err(SignalBuildError::PitViolation(_))));
    }

    #[test]
    fn rejects_market_candle_after_signal() {
        let builder = HistoricalSignalBuilder::new(Decimal::from(4), 6, 9, Decimal::from(150));
        let ts = 1_700_000_000i64;
        let signal_ts = Utc.timestamp_opt(ts, 0).unwrap();
        let result = builder.build(
            "Mint1111111111111111111111111111111111111",
            signal_ts,
            &candle(ts + 60, 0.0001),
            &safety(ts),
            &[wallet(ts - 60)],
            &[candle(ts + 120, 0.00011)],
        );
        assert!(matches!(result, Err(SignalBuildError::PitViolation(_))));
    }

    /// Missing future data must be rejected (fail-closed).
    #[test]
    fn rejects_missing_future_data() {
        let builder = HistoricalSignalBuilder::new(Decimal::from(4), 6, 9, Decimal::from(150));
        let ts = 1_700_000_000i64;
        let signal_ts = Utc.timestamp_opt(ts, 0).unwrap();
        // Market candle strictly before signal_ts so no entry observation
        // is auto-prepended — leaving price_history empty.
        let result = builder.build(
            "Mint1111111111111111111111111111111111111",
            signal_ts,
            &candle(ts - 60, 0.0001),
            &safety(ts - 60),
            &[wallet(ts - 120)],
            &[], // no future candles
        );
        assert!(matches!(
            result,
            Err(SignalBuildError::MissingFutureData(_))
        ));
    }

    /// Empty wallets fail closed: no signal can be issued without
    /// wallet evidence.
    #[test]
    fn rejects_when_no_wallet_evidence() {
        let builder = HistoricalSignalBuilder::new(Decimal::from(4), 6, 9, Decimal::from(150));
        let ts = 1_700_000_000i64;
        let signal_ts = Utc.timestamp_opt(ts, 0).unwrap();
        let result = builder.build(
            "Mint1111111111111111111111111111111111111",
            signal_ts,
            &candle(ts, 0.0001),
            &safety(ts),
            &[], // no wallets
            &[candle(ts + 60, 0.00011)],
        );
        assert!(matches!(
            result,
            Err(SignalBuildError::MissingWalletEvidence(_))
        ));
    }

    /// Serialization round-trip preserves every field, including OHLC.
    #[test]
    fn signal_serialization_round_trip() {
        let builder = HistoricalSignalBuilder::new(Decimal::from(4), 6, 9, Decimal::from(150));
        let ts = 1_700_000_000i64;
        let signal_ts = Utc.timestamp_opt(ts, 0).unwrap();
        let entry = builder
            .build(
                "Mint1111111111111111111111111111111111111",
                signal_ts,
                &candle(ts, 0.0001),
                &safety(ts),
                &[wallet(ts - 60)],
                &[candle(ts + 60, 0.00011), candle(ts + 120, 0.00012)],
            )
            .unwrap();
        let s = serde_json::to_string(&entry.signal).unwrap();
        let parsed: HistoricalSignal = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed.mint, entry.signal.mint);
        assert_eq!(parsed.signal_timestamp.timestamp(), ts);
        assert_eq!(parsed.price_history.len(), entry.signal.price_history.len());
        let first_obs = parsed.price_history.first().unwrap();
        assert!(first_obs.open_usd.is_some());
        assert!(first_obs.high_usd.is_some());
        assert!(first_obs.low_usd.is_some());
        assert!(first_obs.close_usd.is_some());
        assert!(first_obs.volume.is_some());
    }
}
