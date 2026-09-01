use crate::domain::{market::MarketSnapshot, token::TokenSafety, wallet::WalletStats};
use crate::economics::CostModel;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// A single price/liquidity observation at a specific point in time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceObservation {
    pub timestamp: DateTime<Utc>,
    pub price_usd: Decimal,
    pub liquidity_usd: Decimal,
}

/// A historical signal record with full decision context and subsequent price path.
/// This is the JSONL input format for the backtest engine.
///
/// Each record must contain enough information to reproduce the entry decision
/// exactly as it could have been made at signal_timestamp, plus sufficient
/// subsequent price data to determine the actual trade outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoricalSignal {
    /// When this signal was generated. All decision data must be <= this time.
    pub signal_timestamp: DateTime<Utc>,
    /// Token mint address.
    pub mint: String,
    /// Market snapshot at signal time (point-in-time).
    pub market: MarketSnapshot,
    /// Token safety assessment at signal time.
    pub safety: TokenSafety,
    /// Wallet statistics available at signal time (each wallet's updated_at
    /// must be <= signal_timestamp).
    pub wallets: Vec<WalletStats>,
    /// Cost model at signal time.
    pub costs: CostModel,
    /// Intended position size in USD.
    pub position_usd: Decimal,
    /// Model's expected gross return percent. NEVER used in the entry
    /// decision (economic gate, signal score, or acceptance filtering);
    /// recorded for analysis only.
    pub expected_gross_return_pct: Decimal,
    /// Token decimals (typically 6 for SPL tokens).
    pub token_decimals: u8,
    /// Base mint decimals (typically 9 for SOL).
    pub base_mint_decimals: u8,
    /// Subsequent price observations sorted chronologically ascending.
    /// First observation timestamp must be >= signal_timestamp.
    /// Used to determine actual trade outcome (NOT expected_gross_return_pct).
    pub price_history: Vec<PriceObservation>,
}

/// Result of loading and validating historical data.
pub struct LoadResult {
    pub signals: Vec<HistoricalSignal>,
    pub rejected_count: usize,
    pub rejection_reasons: Vec<String>,
}

/// Load historical signals from a JSONL file with strict validation.
pub fn load_historical_signals(path: &Path) -> Result<LoadResult, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let mut signals = Vec::new();
    let mut rejection_reasons = Vec::new();
    let mut rejected_count = 0;

    for (line_num, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<HistoricalSignal>(line) {
            Ok(signal) => match validate_signal(&signal, line_num + 1) {
                Ok(()) => signals.push(signal),
                Err(reason) => {
                    rejected_count += 1;
                    rejection_reasons.push(reason);
                }
            },
            Err(e) => {
                rejected_count += 1;
                rejection_reasons.push(format!("JSON parse error at line {}: {e}", line_num + 1));
            }
        }
    }

    // Sort by signal timestamp to ensure chronological order.
    signals.sort_by_key(|s| s.signal_timestamp);

    Ok(LoadResult {
        signals,
        rejected_count,
        rejection_reasons,
    })
}

fn validate_signal(signal: &HistoricalSignal, line: usize) -> Result<(), String> {
    if signal.price_history.is_empty() {
        return Err(format!(
            "line {line}: price_history is empty; cannot determine trade outcome"
        ));
    }
    for window in signal.price_history.windows(2) {
        if window[0].timestamp > window[1].timestamp {
            return Err(format!(
                "line {line}: price_history is not chronologically sorted"
            ));
        }
    }
    for obs in &signal.price_history {
        if obs.timestamp < signal.signal_timestamp {
            return Err(format!(
                "line {line}: price observation {} precedes signal timestamp {}",
                obs.timestamp, signal.signal_timestamp
            ));
        }
    }
    if signal.position_usd <= Decimal::ZERO {
        return Err(format!("line {line}: position_usd must be positive"));
    }
    // Strict PIT validation: all decision data must be <= signal_timestamp
    if signal.market.observed_at > signal.signal_timestamp {
        return Err(format!(
            "line {line}: market.observed_at ({}) is AFTER signal_timestamp ({}) — look-ahead bias",
            signal.market.observed_at, signal.signal_timestamp
        ));
    }
    if signal.safety.observed_at > signal.signal_timestamp {
        return Err(format!(
            "line {line}: safety.observed_at ({}) is AFTER signal_timestamp ({}) — look-ahead bias",
            signal.safety.observed_at, signal.signal_timestamp
        ));
    }
    if signal.costs.observed_at > signal.signal_timestamp {
        return Err(format!(
            "line {line}: costs.observed_at ({}) is AFTER signal_timestamp ({}) — look-ahead bias",
            signal.costs.observed_at, signal.signal_timestamp
        ));
    }
    for (i, wallet) in signal.wallets.iter().enumerate() {
        if wallet.updated_at > signal.signal_timestamp {
            return Err(format!(
                "line {line}: wallet[{}].updated_at ({}) is AFTER signal_timestamp ({}) — look-ahead bias",
                i, wallet.updated_at, signal.signal_timestamp
            ));
        }
    }
    Ok(())
}

/// Rejection reason with structured data for the rejection summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalRejection {
    pub reason: String,
    pub mint: String,
    pub signal_timestamp: String,
}

/// Pre-filter signals: reject structurally invalid ones before simulation.
///
/// This must NOT consult the wall clock: the entry decision is a pure
/// function of the historical record and the production config, so running
/// the backtest at a different time cannot change any decision.
pub fn prefilter_signals(
    signals: &mut Vec<HistoricalSignal>,
) -> (Vec<HistoricalSignal>, Vec<SignalRejection>) {
    let mut accepted = Vec::new();
    let mut rejected = Vec::new();
    for signal in signals.drain(..) {
        if signal.price_history.is_empty() {
            rejected.push(SignalRejection {
                reason: "empty price_history".into(),
                mint: signal.mint.clone(),
                signal_timestamp: signal.signal_timestamp.to_rfc3339(),
            });
            continue;
        }
        // Mint consistency check
        if signal.market.mint != signal.mint {
            rejected.push(SignalRejection {
                reason: "mint mismatch (market.mint != signal.mint)".into(),
                mint: signal.mint.clone(),
                signal_timestamp: signal.signal_timestamp.to_rfc3339(),
            });
            continue;
        }
        accepted.push(signal);
    }
    (accepted, rejected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use std::io::Write;
    use std::path::PathBuf;

    fn sample_signal_json(signal_timestamp: &str, t1: &str, t2: &str) -> String {
        serde_json::json!({
            "signal_timestamp": signal_timestamp,
            "mint": "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263",
            "market": {
                "mint": "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263",
                "price_usd": "0.0001",
                "liquidity_usd": "100000",
                "volume_24h_usd": "50000",
                "volatility_pct": "15",
                "buy_sell_imbalance": "0.6",
                "observed_at": signal_timestamp,
                "received_at": signal_timestamp
            },
            "safety": {
                "mint_authority_present": false,
                "freeze_authority_present": false,
                "holder_top10_pct": "40",
                "token_age_secs": 172800,
                "sellable": true,
                "route_available": true,
                "observed_at": signal_timestamp
            },
            "wallets": [{
                "wallet": "wallet1",
                "realized_pnl_usd": "1000",
                "win_rate": "0.7",
                "avg_return_pct": "15",
                "median_return_pct": "12",
                "max_drawdown_pct": "20",
                "trades": 50,
                "recent_return_pct": "10",
                "concentration_pct": "5",
                "scam_exposure_pct": "0",
                "score": "80",
                "tier": "Qualified",
                "updated_at": signal_timestamp
            }, {
                "wallet": "wallet2",
                "realized_pnl_usd": "2000",
                "win_rate": "0.65",
                "avg_return_pct": "12",
                "median_return_pct": "10",
                "max_drawdown_pct": "25",
                "trades": 40,
                "recent_return_pct": "8",
                "concentration_pct": "3",
                "scam_exposure_pct": "0",
                "score": "75",
                "tier": "Qualified",
                "updated_at": signal_timestamp
            }],
            "costs": {
                "observed_at": signal_timestamp,
                "input": {
                    "position_size_usd": "4",
                    "avg_priority_fee_usd": "0.002",
                    "avg_swap_fee_bps": "30",
                    "avg_slippage_bps": "50",
                    "avg_price_impact_bps": "20",
                    "failed_tx_rate": "0.05",
                    "avg_failed_tx_cost_usd": "0.002",
                    "assumed_win_loss_ratio": "2",
                    "assumed_avg_loss_pct": "10"
                },
                "source": "backtest",
                "is_live_snapshot": false
            },
            "position_usd": "4",
            "expected_gross_return_pct": "15",
            "token_decimals": 6,
            "base_mint_decimals": 9,
            "price_history": [
                {"timestamp": t1, "price_usd": "0.0001", "liquidity_usd": "100000"},
                {"timestamp": t2, "price_usd": "0.00012", "liquidity_usd": "105000"}
            ]
        })
        .to_string()
    }

    fn write_jsonl(dir: &Path, filename: &str, content: &str) -> PathBuf {
        let path = dir.join(filename);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn load_valid_jsonl() {
        let dir = std::env::temp_dir().join("backtest_data_test_valid");
        let _ = std::fs::create_dir_all(&dir);
        let json = sample_signal_json(
            "2024-01-15T12:00:00Z",
            "2024-01-15T12:00:00Z",
            "2024-01-15T12:05:00Z",
        );
        let path = write_jsonl(&dir, "signals.jsonl", &json);
        let result = load_historical_signals(&path).unwrap();
        assert_eq!(result.signals.len(), 1);
        assert_eq!(result.rejected_count, 0);
        assert_eq!(
            result.signals[0].mint,
            "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reject_empty_price_history() {
        let dir = std::env::temp_dir().join("backtest_data_test_empty");
        let _ = std::fs::create_dir_all(&dir);
        let mut signal: HistoricalSignal = serde_json::from_str(&sample_signal_json(
            "2024-01-15T12:00:00Z",
            "2024-01-15T12:00:00Z",
            "2024-01-15T12:05:00Z",
        ))
        .unwrap();
        signal.price_history = vec![];
        let path = write_jsonl(
            &dir,
            "signals.jsonl",
            &serde_json::to_string(&signal).unwrap(),
        );
        let result = load_historical_signals(&path).unwrap();
        assert_eq!(result.rejected_count, 1);
        assert!(result.rejection_reasons[0].contains("price_history is empty"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reject_unsorted_price_history() {
        let dir = std::env::temp_dir().join("backtest_data_test_unsorted");
        let _ = std::fs::create_dir_all(&dir);
        let mut signal: HistoricalSignal = serde_json::from_str(&sample_signal_json(
            "2024-01-15T12:00:00Z",
            "2024-01-15T12:00:00Z",
            "2024-01-15T12:05:00Z",
        ))
        .unwrap();
        signal.price_history.reverse();
        let path = write_jsonl(
            &dir,
            "signals.jsonl",
            &serde_json::to_string(&signal).unwrap(),
        );
        let result = load_historical_signals(&path).unwrap();
        assert_eq!(result.rejected_count, 1);
        assert!(result.rejection_reasons[0].contains("not chronologically sorted"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reject_negative_position_usd() {
        let dir = std::env::temp_dir().join("backtest_data_test_neg");
        let _ = std::fs::create_dir_all(&dir);
        let mut signal: HistoricalSignal = serde_json::from_str(&sample_signal_json(
            "2024-01-15T12:00:00Z",
            "2024-01-15T12:00:00Z",
            "2024-01-15T12:05:00Z",
        ))
        .unwrap();
        signal.position_usd = dec!(-1);
        let path = write_jsonl(
            &dir,
            "signals.jsonl",
            &serde_json::to_string(&signal).unwrap(),
        );
        let result = load_historical_signals(&path).unwrap();
        assert_eq!(result.rejected_count, 1);
        assert!(result.rejection_reasons[0].contains("position_usd must be positive"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reject_price_observation_before_signal() {
        let dir = std::env::temp_dir().join("backtest_data_test_past");
        let _ = std::fs::create_dir_all(&dir);
        let mut signal: HistoricalSignal = serde_json::from_str(&sample_signal_json(
            "2024-01-15T12:00:00Z",
            "2024-01-15T12:00:00Z",
            "2024-01-15T12:05:00Z",
        ))
        .unwrap();
        signal.price_history[0].timestamp = "2024-01-15T11:59:00Z".parse().unwrap();
        let path = write_jsonl(
            &dir,
            "signals.jsonl",
            &serde_json::to_string(&signal).unwrap(),
        );
        let result = load_historical_signals(&path).unwrap();
        assert_eq!(result.rejected_count, 1);
        assert!(result.rejection_reasons[0].contains("precedes signal timestamp"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn signals_sorted_by_timestamp_after_load() {
        let dir = std::env::temp_dir().join("backtest_data_test_sort");
        let _ = std::fs::create_dir_all(&dir);
        let s1 = sample_signal_json(
            "2024-01-15T13:00:00Z",
            "2024-01-15T13:00:00Z",
            "2024-01-15T13:05:00Z",
        );
        let s2 = sample_signal_json(
            "2024-01-15T12:00:00Z",
            "2024-01-15T12:00:00Z",
            "2024-01-15T12:05:00Z",
        );
        let content = format!("{s1}\n{s2}");
        let path = write_jsonl(&dir, "signals.jsonl", &content);
        let result = load_historical_signals(&path).unwrap();
        assert_eq!(result.signals.len(), 2);
        assert!(result.signals[0].signal_timestamp < result.signals[1].signal_timestamp);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_json_is_rejected() {
        let dir = std::env::temp_dir().join("backtest_data_test_malformed");
        let _ = std::fs::create_dir_all(&dir);
        let path = write_jsonl(&dir, "signals.jsonl", "not valid json {{{");
        let result = load_historical_signals(&path).unwrap();
        assert_eq!(result.rejected_count, 1);
        assert!(result.rejection_reasons[0].contains("JSON parse error"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reject_market_observed_after_signal() {
        let dir = std::env::temp_dir().join("backtest_data_test_market_pit");
        let _ = std::fs::create_dir_all(&dir);
        let mut signal: HistoricalSignal = serde_json::from_str(&sample_signal_json(
            "2024-01-15T12:00:00Z",
            "2024-01-15T12:00:00Z",
            "2024-01-15T12:05:00Z",
        ))
        .unwrap();
        signal.market.observed_at = "2024-01-15T12:01:00Z".parse().unwrap();
        let path = write_jsonl(
            &dir,
            "signals.jsonl",
            &serde_json::to_string(&signal).unwrap(),
        );
        let result = load_historical_signals(&path).unwrap();
        assert_eq!(result.rejected_count, 1);
        assert!(result.rejection_reasons[0].contains("market.observed_at"));
        assert!(result.rejection_reasons[0].contains("look-ahead bias"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reject_safety_observed_after_signal() {
        let dir = std::env::temp_dir().join("backtest_data_test_safety_pit");
        let _ = std::fs::create_dir_all(&dir);
        let mut signal: HistoricalSignal = serde_json::from_str(&sample_signal_json(
            "2024-01-15T12:00:00Z",
            "2024-01-15T12:00:00Z",
            "2024-01-15T12:05:00Z",
        ))
        .unwrap();
        signal.safety.observed_at = "2024-01-15T12:01:00Z".parse().unwrap();
        let path = write_jsonl(
            &dir,
            "signals.jsonl",
            &serde_json::to_string(&signal).unwrap(),
        );
        let result = load_historical_signals(&path).unwrap();
        assert_eq!(result.rejected_count, 1);
        assert!(result.rejection_reasons[0].contains("safety.observed_at"));
        assert!(result.rejection_reasons[0].contains("look-ahead bias"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reject_wallet_updated_after_signal() {
        let dir = std::env::temp_dir().join("backtest_data_test_wallet_pit");
        let _ = std::fs::create_dir_all(&dir);
        let mut signal: HistoricalSignal = serde_json::from_str(&sample_signal_json(
            "2024-01-15T12:00:00Z",
            "2024-01-15T12:00:00Z",
            "2024-01-15T12:05:00Z",
        ))
        .unwrap();
        signal.wallets[0].updated_at = "2024-01-15T12:01:00Z".parse().unwrap();
        let path = write_jsonl(
            &dir,
            "signals.jsonl",
            &serde_json::to_string(&signal).unwrap(),
        );
        let result = load_historical_signals(&path).unwrap();
        assert_eq!(result.rejected_count, 1);
        assert!(result.rejection_reasons[0].contains("wallet[0].updated_at"));
        assert!(result.rejection_reasons[0].contains("look-ahead bias"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reject_costs_observed_after_signal() {
        let dir = std::env::temp_dir().join("backtest_data_test_costs_pit");
        let _ = std::fs::create_dir_all(&dir);
        let mut signal: HistoricalSignal = serde_json::from_str(&sample_signal_json(
            "2024-01-15T12:00:00Z",
            "2024-01-15T12:00:00Z",
            "2024-01-15T12:05:00Z",
        ))
        .unwrap();
        signal.costs.observed_at = "2024-01-15T12:01:00Z".parse().unwrap();
        let path = write_jsonl(
            &dir,
            "signals.jsonl",
            &serde_json::to_string(&signal).unwrap(),
        );
        let result = load_historical_signals(&path).unwrap();
        assert_eq!(result.rejected_count, 1);
        assert!(result.rejection_reasons[0].contains("costs.observed_at"));
        assert!(result.rejection_reasons[0].contains("look-ahead bias"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reject_mint_mismatch() {
        let dir = std::env::temp_dir().join("backtest_data_test_mint_mismatch");
        let _ = std::fs::create_dir_all(&dir);
        let mut signal: HistoricalSignal = serde_json::from_str(&sample_signal_json(
            "2024-01-15T12:00:00Z",
            "2024-01-15T12:00:00Z",
            "2024-01-15T12:05:00Z",
        ))
        .unwrap();
        signal.market.mint = "WRONG_MINT_ADDRESS".into();
        let path = write_jsonl(
            &dir,
            "signals.jsonl",
            &serde_json::to_string(&signal).unwrap(),
        );
        let mut result = load_historical_signals(&path).unwrap();
        let (accepted, rejected) = prefilter_signals(&mut result.signals);
        assert_eq!(accepted.len(), 0);
        assert_eq!(rejected.len(), 1);
        assert!(rejected[0].reason.contains("mint mismatch"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
