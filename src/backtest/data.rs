use crate::domain::{market::MarketSnapshot, token::TokenSafety, wallet::WalletStats};
use crate::economics::CostModel;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

/// A single OHLCV+liquidity observation at a specific point in time.
///
/// For backward compatibility, `price_usd` is always present and treated
/// as the close price. When `open_usd`, `high_usd`, `low_usd`, `close_usd`,
/// and `volume` are absent, they are derived from `price_usd` so that
/// close-price-only datasets remain valid without modification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceObservation {
    pub timestamp: DateTime<Utc>,
    /// Close price (always present; legacy field).
    pub price_usd: Decimal,
    pub liquidity_usd: Decimal,
    /// Open price for the candle. Defaults to `price_usd` when absent.
    #[serde(default)]
    pub open_usd: Option<Decimal>,
    /// High price for the candle. Defaults to `price_usd` when absent.
    #[serde(default)]
    pub high_usd: Option<Decimal>,
    /// Low price for the candle. Defaults to `price_usd` when absent.
    #[serde(default)]
    pub low_usd: Option<Decimal>,
    /// Close price for the candle. Defaults to `price_usd` when absent.
    #[serde(default)]
    pub close_usd: Option<Decimal>,
    /// Volume traded during the candle (optional, informational).
    #[serde(default)]
    pub volume: Option<Decimal>,
}

impl PriceObservation {
    /// Effective open price: explicit OHLC open, or the close price.
    pub fn effective_open(&self) -> Decimal {
        self.open_usd.unwrap_or(self.price_usd)
    }

    /// Effective high price: explicit OHLC high, or the close price.
    pub fn effective_high(&self) -> Decimal {
        self.high_usd.unwrap_or(self.price_usd)
    }

    /// Effective low price: explicit OHLC low, or the close price.
    pub fn effective_low(&self) -> Decimal {
        self.low_usd.unwrap_or(self.price_usd)
    }

    /// Effective close price: explicit OHLC close, or the close price.
    pub fn effective_close(&self) -> Decimal {
        self.close_usd.unwrap_or(self.price_usd)
    }
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
///
/// The validation is fail-closed: any structural problem rejects the
/// record. This protects the backtest from look-ahead bias, malformed
/// inputs, and survivorship-bias sampling.
pub fn load_historical_signals(path: &Path) -> Result<LoadResult, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let mut signals = Vec::new();
    let mut rejection_reasons = Vec::new();
    let mut rejected_count = 0;
    let mut seen_signal_keys: HashSet<(String, DateTime<Utc>)> = HashSet::new();

    for (line_num, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match serde_json::from_str::<HistoricalSignal>(line) {
            Ok(signal) => match validate_signal(&signal, line_num + 1) {
                Ok(()) => {
                    // Reject duplicate signals: same (mint, signal_timestamp)
                    // is the same observation, regardless of body.
                    let key = (signal.mint.clone(), signal.signal_timestamp);
                    if !seen_signal_keys.insert(key) {
                        rejected_count += 1;
                        rejection_reasons.push(format!(
                            "line {}: duplicate signal (same mint + signal_timestamp)",
                            line_num + 1
                        ));
                        continue;
                    }
                    signals.push(signal);
                }
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

/// Loose Solana base58 mint check: 32-44 chars from the base58
/// alphabet (excluding 0/O/I/l for human-readability reasons). Not a
/// full base58 decode, but enough to reject empty strings, whitespace,
/// and clearly invalid inputs. Combined with the 32-44 char length
/// check, this catches the most common data-entry mistakes.
fn is_valid_solana_mint(s: &str) -> bool {
    let len = s.len();
    if !(32..=44).contains(&len) {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() && c != '0' && c != 'O' && c != 'I' && c != 'l')
}

/// Strict, fail-closed structural + PIT validation. Every check that
/// could allow a look-ahead bias, malformed observation, or invalid
/// value REJECTS the record.
fn validate_signal(signal: &HistoricalSignal, line: usize) -> Result<(), String> {
    if signal.price_history.is_empty() {
        return Err(format!(
            "line {line}: price_history is empty; cannot determine trade outcome"
        ));
    }
    if !is_valid_solana_mint(&signal.mint) {
        return Err(format!(
            "line {line}: invalid mint address {:?} (expected 32-44 base58 chars)",
            signal.mint
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
        if obs.price_usd <= Decimal::ZERO {
            return Err(format!(
                "line {line}: price observation {} has non-positive price_usd {}",
                obs.timestamp, obs.price_usd
            ));
        }
        if obs.liquidity_usd < Decimal::ZERO {
            return Err(format!(
                "line {line}: price observation {} has negative liquidity_usd {}",
                obs.timestamp, obs.liquidity_usd
            ));
        }
        // Validate OHLC consistency when explicit fields are present.
        if let Some(open) = obs.open_usd {
            if open <= Decimal::ZERO {
                return Err(format!(
                    "line {line}: price observation {} has non-positive open_usd {}",
                    obs.timestamp, open
                ));
            }
        }
        if let Some(high) = obs.high_usd {
            if high <= Decimal::ZERO {
                return Err(format!(
                    "line {line}: price observation {} has non-positive high_usd {}",
                    obs.timestamp, high
                ));
            }
        }
        if let Some(low) = obs.low_usd {
            if low <= Decimal::ZERO {
                return Err(format!(
                    "line {line}: price observation {} has non-positive low_usd {}",
                    obs.timestamp, low
                ));
            }
        }
        if let Some(close) = obs.close_usd {
            if close <= Decimal::ZERO {
                return Err(format!(
                    "line {line}: price observation {} has non-positive close_usd {}",
                    obs.timestamp, close
                ));
            }
        }
        // OHLC consistency: high >= low.
        let eff_high = obs.effective_high();
        let eff_low = obs.effective_low();
        if eff_high < eff_low {
            return Err(format!(
                "line {line}: price observation {} has high ({}) < low ({})",
                obs.timestamp, eff_high, eff_low
            ));
        }
    }
    if signal.position_usd <= Decimal::ZERO {
        return Err(format!("line {line}: position_usd must be positive"));
    }
    if signal.expected_gross_return_pct < Decimal::ZERO {
        return Err(format!(
            "line {line}: expected_gross_return_pct must be non-negative"
        ));
    }
    if signal.token_decimals > 18 {
        return Err(format!(
            "line {line}: token_decimals ({}) implausibly large (max 18)",
            signal.token_decimals
        ));
    }
    if signal.base_mint_decimals > 18 {
        return Err(format!(
            "line {line}: base_mint_decimals ({}) implausibly large (max 18)",
            signal.base_mint_decimals
        ));
    }
    // Strict PIT validation: all decision data must be <= signal_timestamp.
    if signal.market.observed_at > signal.signal_timestamp {
        return Err(format!(
            "line {line}: market.observed_at ({}) is AFTER signal_timestamp ({}) — look-ahead bias",
            signal.market.observed_at, signal.signal_timestamp
        ));
    }
    if signal.market.price_usd <= Decimal::ZERO {
        return Err(format!("line {line}: market.price_usd must be positive"));
    }
    if signal.market.liquidity_usd < Decimal::ZERO {
        return Err(format!(
            "line {line}: market.liquidity_usd must be non-negative"
        ));
    }
    if signal.safety.observed_at > signal.signal_timestamp {
        return Err(format!(
            "line {line}: safety.observed_at ({}) is AFTER signal_timestamp ({}) — look-ahead bias",
            signal.safety.observed_at, signal.signal_timestamp
        ));
    }
    if signal.safety.token_age_secs < 0 {
        return Err(format!(
            "line {line}: safety.token_age_secs must be non-negative"
        ));
    }
    if signal.costs.observed_at > signal.signal_timestamp {
        return Err(format!(
            "line {line}: costs.observed_at ({}) is AFTER signal_timestamp ({}) — look-ahead bias",
            signal.costs.observed_at, signal.signal_timestamp
        ));
    }
    // Wallet PIT: every wallet.updated_at must be <= signal_timestamp
    // AND <= market.observed_at (the strictest PIT anchor). Wallets
    // updated after the market snapshot was taken cannot have informed
    // the entry decision.
    if signal.wallets.is_empty() {
        return Err(format!("line {line}: no wallets in signal"));
    }
    for (i, wallet) in signal.wallets.iter().enumerate() {
        if wallet.updated_at > signal.signal_timestamp {
            return Err(format!(
                "line {line}: wallet[{}].updated_at ({}) is AFTER signal_timestamp ({}) — look-ahead bias",
                i, wallet.updated_at, signal.signal_timestamp
            ));
        }
        if wallet.updated_at > signal.market.observed_at {
            return Err(format!(
                "line {line}: wallet[{}].updated_at ({}) is AFTER market.observed_at ({}) — wallet data was not yet available when the market was observed",
                i, wallet.updated_at, signal.market.observed_at
            ));
        }
        if !is_valid_solana_pubkey(&wallet.wallet) {
            return Err(format!(
                "line {line}: wallet[{}].wallet ({:?}) is not a valid Solana pubkey",
                i, wallet.wallet
            ));
        }
    }
    Ok(())
}

/// Loose Solana base58 pubkey check for wallet addresses.
fn is_valid_solana_pubkey(s: &str) -> bool {
    is_valid_solana_mint(s)
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
                "wallet": "5kqEvH3gnx5HUYA8UmK3Za5gF3kRpY3oUg3TCY4tJhPb",
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
                "wallet": "7xKXtg2CW87d97TXJSDpbD5jBkheTqA83TZRuJosgAsU",
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

    // =========================================================================
    // OHLC-related regression tests
    // =========================================================================

    #[test]
    fn ohlc_fields_default_to_close_when_absent() {
        let obs = PriceObservation {
            timestamp: "2024-01-15T12:05:00Z".parse().unwrap(),
            price_usd: dec!(0.0001),
            liquidity_usd: dec!(100000),
            open_usd: None,
            high_usd: None,
            low_usd: None,
            close_usd: None,
            volume: None,
        };
        assert_eq!(obs.effective_open(), dec!(0.0001));
        assert_eq!(obs.effective_high(), dec!(0.0001));
        assert_eq!(obs.effective_low(), dec!(0.0001));
        assert_eq!(obs.effective_close(), dec!(0.0001));
    }

    #[test]
    fn ohlc_fields_use_explicit_values_when_present() {
        let obs = PriceObservation {
            timestamp: "2024-01-15T12:05:00Z".parse().unwrap(),
            price_usd: dec!(0.0001),
            liquidity_usd: dec!(100000),
            open_usd: Some(dec!(0.000099)),
            high_usd: Some(dec!(0.000115)),
            low_usd: Some(dec!(0.000094)),
            close_usd: Some(dec!(0.000102)),
            volume: Some(dec!(50000)),
        };
        assert_eq!(obs.effective_open(), dec!(0.000099));
        assert_eq!(obs.effective_high(), dec!(0.000115));
        assert_eq!(obs.effective_low(), dec!(0.000094));
        assert_eq!(obs.effective_close(), dec!(0.000102));
    }

    #[test]
    fn reject_high_less_than_low() {
        let dir = std::env::temp_dir().join("backtest_data_test_ohlc_invalid");
        let _ = std::fs::create_dir_all(&dir);
        let mut signal: HistoricalSignal = serde_json::from_str(&sample_signal_json(
            "2024-01-15T12:00:00Z",
            "2024-01-15T12:00:00Z",
            "2024-01-15T12:05:00Z",
        ))
        .unwrap();
        signal.price_history[0].high_usd = Some(dec!(0.00009));
        signal.price_history[0].low_usd = Some(dec!(0.00011));
        let path = write_jsonl(
            &dir,
            "signals.jsonl",
            &serde_json::to_string(&signal).unwrap(),
        );
        let result = load_historical_signals(&path).unwrap();
        assert_eq!(result.rejected_count, 1);
        assert!(result.rejection_reasons[0].contains("high"));
        assert!(result.rejection_reasons[0].contains("low"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reject_non_positive_ohlc_fields() {
        let dir = std::env::temp_dir().join("backtest_data_test_ohlc_nonpos");
        let _ = std::fs::create_dir_all(&dir);
        let mut signal: HistoricalSignal = serde_json::from_str(&sample_signal_json(
            "2024-01-15T12:00:00Z",
            "2024-01-15T12:00:00Z",
            "2024-01-15T12:05:00Z",
        ))
        .unwrap();
        signal.price_history[0].open_usd = Some(dec!(-1));
        let path = write_jsonl(
            &dir,
            "signals.jsonl",
            &serde_json::to_string(&signal).unwrap(),
        );
        let result = load_historical_signals(&path).unwrap();
        assert_eq!(result.rejected_count, 1);
        assert!(result.rejection_reasons[0].contains("open_usd"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ohlc_fields_round_trip_through_jsonl() {
        let dir = std::env::temp_dir().join("backtest_data_test_ohlc_roundtrip");
        let _ = std::fs::create_dir_all(&dir);
        let mut signal: HistoricalSignal = serde_json::from_str(&sample_signal_json(
            "2024-01-15T12:00:00Z",
            "2024-01-15T12:00:00Z",
            "2024-01-15T12:05:00Z",
        ))
        .unwrap();
        signal.price_history[0].open_usd = Some(dec!(0.000099));
        signal.price_history[0].high_usd = Some(dec!(0.000115));
        signal.price_history[0].low_usd = Some(dec!(0.000094));
        signal.price_history[0].close_usd = Some(dec!(0.000102));
        signal.price_history[0].volume = Some(dec!(50000));
        let path = write_jsonl(
            &dir,
            "signals.jsonl",
            &serde_json::to_string(&signal).unwrap(),
        );
        let result = load_historical_signals(&path).unwrap();
        assert_eq!(result.signals.len(), 1);
        let obs = &result.signals[0].price_history[0];
        assert_eq!(obs.open_usd, Some(dec!(0.000099)));
        assert_eq!(obs.high_usd, Some(dec!(0.000115)));
        assert_eq!(obs.low_usd, Some(dec!(0.000094)));
        assert_eq!(obs.close_usd, Some(dec!(0.000102)));
        assert_eq!(obs.volume, Some(dec!(50000)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
