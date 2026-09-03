//! Strict, fail-closed validator for the real historical dataset.
//!
//! Performs every check the requirement list calls for: JSON validity,
//! chronological ordering, duplicate signals, valid mint, valid
//! timestamps, positive prices, valid OHLC, PIT consistency (wallets,
//! market, safety, costs), future price observations strictly after
//! entry when appropriate, sufficient future data, position-size vs
//! cost-model consistency.

use crate::backtest::data::{load_historical_signals, HistoricalSignal};
use chrono::{DateTime, Utc};
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct HistoricalValidationReport {
    pub path: PathBuf,
    pub records: usize,
    pub valid_records: usize,
    pub rejected_records: usize,
    pub pit_violations: usize,
    pub missing_data_records: usize,
    pub duplicate_records: usize,
    pub chronological_order_ok: bool,
    pub date_range: Option<(DateTime<Utc>, DateTime<Utc>)>,
    pub unique_tokens: usize,
    pub unique_wallets: usize,
    pub issues: Vec<String>,
}

impl HistoricalValidationReport {
    pub fn is_clean(&self) -> bool {
        self.rejected_records == 0
            && self.pit_violations == 0
            && self.missing_data_records == 0
            && self.duplicate_records == 0
            && self.chronological_order_ok
            && self.issues.is_empty()
    }
}

pub struct HistoricalValidator {
    path: PathBuf,
    strict: bool,
}

impl HistoricalValidator {
    pub fn new(path: PathBuf) -> Self {
        Self { path, strict: true }
    }

    pub fn non_strict(mut self) -> Self {
        self.strict = false;
        self
    }

    /// Run the full validation. When `strict` is true (default),
    /// any issue produces a non-clean report.
    pub fn validate(&self) -> Result<HistoricalValidationReport, String> {
        let content = std::fs::read_to_string(&self.path)
            .map_err(|e| format!("read {}: {e}", self.path.display()))?;
        let mut signals: Vec<HistoricalSignal> = Vec::new();
        let mut json_invalid = 0usize;
        let mut issues = Vec::new();
        let mut seen_keys: HashSet<(String, DateTime<Utc>)> = HashSet::new();
        let mut duplicate_count = 0usize;
        for (i, line) in content.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            match serde_json::from_str::<HistoricalSignal>(line) {
                Ok(s) => {
                    if !seen_keys.insert((s.mint.clone(), s.signal_timestamp)) {
                        duplicate_count += 1;
                        issues.push(format!(
                            "line {}: duplicate signal ({} @ {})",
                            i + 1,
                            s.mint,
                            s.signal_timestamp.to_rfc3339()
                        ));
                        continue;
                    }
                    signals.push(s);
                }
                Err(e) => {
                    json_invalid += 1;
                    issues.push(format!("line {}: invalid JSON: {e}", i + 1));
                }
            }
        }
        // Use the production loader for the load-time PIT and structural
        // checks; we count issues ourselves.
        let load_result = load_historical_signals(&self.path)?;
        let pit_violations = load_result.rejection_reasons.len();
        for r in &load_result.rejection_reasons {
            issues.push(format!("load-time rejection: {r}"));
        }
        let sorted = load_result.signals;
        let mut chronological = true;
        for w in sorted.windows(2) {
            if w[0].signal_timestamp > w[1].signal_timestamp {
                chronological = false;
                issues.push("signals not in chronological order".into());
                break;
            }
        }
        let mut missing_data = 0usize;
        for s in &sorted {
            if s.price_history.is_empty() {
                missing_data += 1;
                issues.push(format!("{}: empty price_history", s.signal_timestamp));
            }
            // First observation must be >= signal_timestamp.
            if let Some(first) = s.price_history.first() {
                if first.timestamp < s.signal_timestamp {
                    missing_data += 1;
                    issues.push(format!(
                        "{}: price_history[0].timestamp precedes signal_timestamp",
                        s.signal_timestamp
                    ));
                }
            }
            // Position size vs cost model consistency.
            if s.costs.input.position_size_usd != s.position_usd {
                missing_data += 1;
                issues.push(format!(
                    "{}: costs.input.position_size_usd ({}) != signal.position_usd ({})",
                    s.signal_timestamp, s.costs.input.position_size_usd, s.position_usd
                ));
            }
        }
        let unique_tokens: HashSet<String> = sorted.iter().map(|s| s.mint.clone()).collect();
        let unique_wallets: HashSet<String> = sorted
            .iter()
            .flat_map(|s| s.wallets.iter().map(|w| w.wallet.clone()))
            .collect();
        let date_range = if sorted.is_empty() {
            None
        } else {
            Some((
                sorted.first().unwrap().signal_timestamp,
                sorted.last().unwrap().signal_timestamp,
            ))
        };
        let valid_records = sorted.len();
        let total_records = signals.len() + json_invalid;
        let rejected_records = total_records - valid_records;
        let report = HistoricalValidationReport {
            path: self.path.clone(),
            records: total_records,
            valid_records,
            rejected_records,
            pit_violations,
            missing_data_records: missing_data,
            duplicate_records: duplicate_count,
            chronological_order_ok: chronological,
            date_range,
            unique_tokens: unique_tokens.len(),
            unique_wallets: unique_wallets.len(),
            issues,
        };
        if self.strict && !report.is_clean() {
            // Print the report first so the operator can see exactly
            // what failed before the error code.
            print_summary(&report);
            return Err(format!(
                "validation failed: rejected={}, pit={}, missing={}, dup={}, chrono_ok={}",
                report.rejected_records,
                report.pit_violations,
                report.missing_data_records,
                report.duplicate_records,
                report.chronological_order_ok
            ));
        }
        Ok(report)
    }
}

/// Print a concise summary report.
pub fn print_summary(report: &HistoricalValidationReport) {
    println!("=== Real Historical Dataset Validation ===");
    println!("path:                    {}", report.path.display());
    println!("records:                 {}", report.records);
    if let Some((from, to)) = report.date_range {
        println!(
            "date range:              {} → {}",
            from.to_rfc3339(),
            to.to_rfc3339()
        );
    } else {
        println!("date range:              <empty>");
    }
    println!("unique tokens:           {}", report.unique_tokens);
    println!("unique wallets:          {}", report.unique_wallets);
    println!("valid records:           {}", report.valid_records);
    println!("rejected records:        {}", report.rejected_records);
    println!("PIT violations:          {}", report.pit_violations);
    println!("missing-data records:    {}", report.missing_data_records);
    println!("duplicate records:       {}", report.duplicate_records);
    println!(
        "chronological order:     {}",
        if report.chronological_order_ok {
            "ok"
        } else {
            "OUT OF ORDER"
        }
    );
    if !report.issues.is_empty() {
        println!("first issues (max 10):");
        for i in report.issues.iter().take(10) {
            println!("  - {i}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backtest::data::{HistoricalSignal, PriceObservation};
    use crate::domain::market::MarketSnapshot;
    use crate::domain::token::TokenSafety;
    use crate::domain::wallet::{WalletStats, WalletTier};
    use crate::economics::{BreakEvenInputs, CostModel};
    use chrono::TimeZone;
    use rust_decimal_macros::dec;
    use std::io::Write;
    use tempfile::tempdir;

    fn sample_signal(signal_ts: i64, history: Vec<(i64, f64)>) -> HistoricalSignal {
        let ts = Utc.timestamp_opt(signal_ts, 0).unwrap();
        HistoricalSignal {
            signal_timestamp: ts,
            mint: "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263".into(),
            market: MarketSnapshot {
                mint: "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263".into(),
                price_usd: dec!(0.0001),
                liquidity_usd: dec!(100000),
                volume_24h_usd: dec!(50000),
                volatility_pct: dec!(15),
                buy_sell_imbalance: dec!(0.6),
                observed_at: ts,
                received_at: ts,
                slot: None,
                price_impact_bps: None,
            },
            safety: TokenSafety {
                mint_authority_present: false,
                freeze_authority_present: false,
                holder_top10_pct: dec!(40),
                token_age_secs: 86400 * 3,
                liquidity_locked_or_burned: Some(true),
                sellable: Some(true),
                route_available: Some(true),
                creator_suspicious: Some(false),
                abnormal_activity: Some(false),
                liquidity_change_pct: Some(dec!(0)),
                observed_at: ts,
            },
            wallets: vec![WalletStats {
                wallet: "5kqEvH3gnx5HUYA8UmK3Za5gF3kRpY3oUg3TCY4tJhPb".into(),
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
                score: dec!(80),
                tier: WalletTier::Qualified,
                updated_at: ts,
            }],
            costs: CostModel {
                observed_at: ts,
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
            },
            position_usd: dec!(4),
            expected_gross_return_pct: dec!(15),
            token_decimals: 6,
            base_mint_decimals: 9,
            price_history: history
                .into_iter()
                .map(|(t, p)| PriceObservation {
                    timestamp: Utc.timestamp_opt(t, 0).unwrap(),
                    price_usd: rust_decimal::Decimal::from_f64_retain(p).unwrap(),
                    liquidity_usd: dec!(100000),
                    open_usd: None,
                    high_usd: None,
                    low_usd: None,
                    close_usd: None,
                    volume: None,
                })
                .collect(),
        }
    }

    #[test]
    fn accepts_clean_dataset() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ok.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        let s1 = sample_signal(1_700_000_000, vec![(1_700_000_300, 0.00011)]);
        let s2 = sample_signal(1_700_001_000, vec![(1_700_001_300, 0.00011)]);
        for s in [s1, s2] {
            writeln!(f, "{}", serde_json::to_string(&s).unwrap()).unwrap();
        }
        let report = HistoricalValidator::new(path).validate().unwrap();
        assert_eq!(report.valid_records, 2);
        assert_eq!(report.rejected_records, 0);
    }

    #[test]
    fn detects_duplicates() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("dup.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        let s = sample_signal(1_700_000_000, vec![(1_700_000_300, 0.00011)]);
        for _ in 0..2 {
            writeln!(f, "{}", serde_json::to_string(&s).unwrap()).unwrap();
        }
        let report = HistoricalValidator::new(path)
            .non_strict()
            .validate()
            .unwrap();
        assert!(report.duplicate_records >= 1 || report.rejected_records >= 1);
    }

    #[test]
    fn detects_position_cost_mismatch() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mismatch.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        let mut s = sample_signal(1_700_000_000, vec![(1_700_000_300, 0.00011)]);
        s.position_usd = dec!(5);
        writeln!(f, "{}", serde_json::to_string(&s).unwrap()).unwrap();
        let report = HistoricalValidator::new(path)
            .non_strict()
            .validate()
            .unwrap();
        assert!(report.missing_data_records >= 1 || report.rejected_records >= 1);
    }

    #[test]
    fn detects_pit_violation() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("pit.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        let mut s = sample_signal(1_700_000_000, vec![(1_700_000_300, 0.00011)]);
        s.wallets[0].updated_at = Utc.timestamp_opt(1_700_000_500, 0).unwrap();
        writeln!(f, "{}", serde_json::to_string(&s).unwrap()).unwrap();
        let report = HistoricalValidator::new(path)
            .non_strict()
            .validate()
            .unwrap();
        assert!(report.pit_violations >= 1 || report.rejected_records >= 1);
    }
}
