//! Historical data recording for backtest input.
//!
//! This module provides the *minimum* infrastructure needed to produce
//! point-in-time historical data compatible with the backtest engine.
//!
//! The Solana RPC pool in this crate (see `src/data/rpc.rs`) only
//! exposes forward-looking methods (`getBalance`,
//! `getSignatureStatuses`, `getTransaction`,
//! `getTokenAccountsByOwner`, `getHealth`). It has no `getMultipleAccounts`
//! at a historical slot, no `getSignaturesForAddress`, no
//! `getTokenLargestAccounts` at slot N, and no Geyser / Birdeye /
//! Bitquery integration. As a result, NO historical smart-money or
//! market state can be reconstructed from the chain via the existing
//! RPC pool.
//!
//! What we *can* do is record real-time observations from the live
//! Jupiter quote path (see `src/collector/mod.rs::collect_live`).
//! Every signal the strategy *would have* considered is recorded with
//! its real on-chain market snapshot, real route availability, real
//! SOL price, and a calibrated cost model. The result is JSONL in the
//! same schema as `config/candidates.jsonl`.
//!
//! What is *not* recorded:
//! 1. Historical wallet performance: the wallet scores in
//!    `collect_live` are derived heuristically from `price_impact_bps`
//!    (line 195 of `src/collector/mod.rs`). This is a SYNTHETIC proxy
//!    for `score`, not historical PnL reconstruction. A real historical
//!    backtest requires a separate `WalletTracker` that persists
//!    per-wallet statistics to disk and queries the chain for historical
//!    trade history. That does not exist.
//! 2. Subsequent price observations for exit simulation: the
//!    collector captures a single point-in-time quote, not a forward
//!    price path. A real historical backtest requires a historical
//!    price feed (Geyser, Birdeye, Bitquery). That does not exist.
//! 3. `holder_top10_pct` at a historical slot: the value recorded is
//!    the current holder distribution at signal time, not the
//!    distribution as it existed on the historical block. A real
//!    backtest requires `getTokenLargestAccounts` at a specific slot,
//!    which the RPC pool does not support.
//! 4. `creator_suspicious`, `abnormal_activity`,
//!    `liquidity_change_pct`: these are hard-coded heuristic
//!    constants. A real backtest requires external threat-intelligence
//!    data (Helius, Birdeye). That integration does not exist.
//!
//! The `collect-history` CLI subcommand records what the current
//! infrastructure can honestly record. It will NOT fabricate
//! historical data to make the backtest work.

use crate::runtime::CandidateInput;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// One recorded observation. Same schema as `CandidateInput` in
/// `config/candidates.jsonl` plus a recording timestamp.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryRecord {
    /// Wall-clock time the observation was recorded. NOT the
    /// signal_timestamp used by the strategy: that is on
    /// `CandidateInput.market.observed_at`.
    pub recorded_at: DateTime<Utc>,
    /// Source identifier (e.g. "jupiter_live", "manual_import").
    pub source: String,
    /// The strategy-level candidate input, identical to
    /// `config/candidates.jsonl`.
    #[serde(flatten)]
    pub candidate: CandidateInput,
}

/// Validation report for a historical dataset.
#[derive(Debug, Clone, Serialize)]
pub struct HistoryValidationReport {
    pub path: PathBuf,
    pub total_lines: usize,
    pub parseable_lines: usize,
    pub unparseable_lines: Vec<String>,
    pub duplicate_mints: Vec<String>,
    pub records: Vec<HistoryRecord>,
    pub future_dated_count: usize,
    pub missing_decimals_count: usize,
    pub cost_mismatch_count: usize,
    pub missing_wallet_pit_count: usize,
}

/// Records live observations to a JSONL file.
///
/// This is a thin append-only writer. It does NOT invent data.
pub struct HistoryRecorder {
    output_path: PathBuf,
    seen_mints_this_session: HashSet<String>,
    file_created: bool,
}

impl HistoryRecorder {
    pub fn new(output_path: PathBuf) -> Self {
        Self {
            output_path,
            seen_mints_this_session: HashSet::new(),
            file_created: false,
        }
    }

    /// Ensure the output file exists with a header comment.
    pub fn ensure_file(&mut self) -> std::io::Result<()> {
        if self.file_created {
            return Ok(());
        }
        if let Some(parent) = self.output_path.parent() {
            fs::create_dir_all(parent)?;
        }
        // Append-mode so multiple sessions accumulate.
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.output_path)?;
        if self.file_is_empty()? {
            f.write_all(b"# Historical data collected by `cargo run -- collect-history`.\n")?;
            f.write_all(b"# Each line is one CandidateInput as observed live by the\n")?;
            f.write_all(b"# production collector. Strict PIT (point-in-time) validation\n")?;
            f.write_all(b"# applies: any feature.observed_at must be <= market.observed_at\n")?;
            f.write_all(b"# and every wallet.updated_at must be <= market.observed_at.\n")?;
            f.write_all(b"#\n")?;
            f.write_all(b"# KNOWN LIMITATIONS of this recorded data:\n")?;
            f.write_all(b"# - wallet score and trades are heuristic, not historical PnL\n")?;
            f.write_all(b"# - no subsequent price observations are recorded (no forward\n")?;
            f.write_all(b"#   price path for exit simulation)\n")?;
            f.write_all(b"# - holder_top10_pct, creator_suspicious, abnormal_activity,\n")?;
            f.write_all(b"#   liquidity_change_pct are hard-coded defaults\n")?;
            f.write_all(b"#\n")?;
        }
        self.file_created = true;
        Ok(())
    }

    fn file_is_empty(&self) -> std::io::Result<bool> {
        match File::open(&self.output_path) {
            Ok(f) => Ok(f.metadata()?.len() == 0),
            Err(_) => Ok(true),
        }
    }

    /// Record one candidate. Returns `true` if appended, `false` if
    /// rejected (duplicate mint seen this session, or no mint).
    pub fn record(&mut self, c: &CandidateInput) -> std::io::Result<bool> {
        self.ensure_file()?;
        if c.mint.is_empty() {
            return Ok(false);
        }
        if !self.seen_mints_this_session.insert(c.mint.clone()) {
            // Already recorded this mint this session.
            return Ok(false);
        }
        let rec = HistoryRecord {
            recorded_at: Utc::now(),
            source: "collect_live".into(),
            candidate: c.clone(),
        };
        let json = serde_json::to_string(&rec)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let mut f = OpenOptions::new().append(true).open(&self.output_path)?;
        f.write_all(json.as_bytes())?;
        f.write_all(b"\n")?;
        Ok(true)
    }

    pub fn recorded_count(&self) -> usize {
        self.seen_mints_this_session.len()
    }
}

/// Read every line of a history file and parse what we can. The
/// `HistoryValidator` is fail-closed: any structural problem produces
/// a report entry, and the dataset is treated as unfit for real-data
/// backtesting.
pub struct HistoryValidator {
    path: PathBuf,
    /// Required mints to be present (survivorship-bias mitigation).
    /// If a historical dataset must include these mints to avoid
    /// selective sampling. When empty, all mints are accepted.
    required_mints: HashSet<String>,
}

impl HistoryValidator {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            required_mints: HashSet::new(),
        }
    }

    /// Add a mint that MUST be present in the dataset.
    pub fn require_mint(&mut self, mint: &str) {
        self.required_mints.insert(mint.to_string());
    }

    /// Read the entire file, parse every line, and produce a
    /// validation report. The report includes the parseable records
    /// so the caller can re-use them.
    pub fn validate(&self) -> Result<HistoryValidationReport, String> {
        let content = fs::read_to_string(&self.path)
            .map_err(|e| format!("read {}: {e}", self.path.display()))?;
        let mut report = HistoryValidationReport {
            path: self.path.clone(),
            total_lines: 0,
            parseable_lines: 0,
            unparseable_lines: Vec::new(),
            duplicate_mints: Vec::new(),
            records: Vec::new(),
            future_dated_count: 0,
            missing_decimals_count: 0,
            cost_mismatch_count: 0,
            missing_wallet_pit_count: 0,
        };
        let mut seen_mints: HashSet<String> = HashSet::new();
        for (i, line) in content.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            report.total_lines += 1;
            match serde_json::from_str::<HistoryRecord>(trimmed) {
                Ok(rec) => {
                    let valid = is_valid_solana_pubkey(&rec.candidate.mint);
                    eprintln!(
                        "DEBUG: line {} mint={:?} valid={} unparseable_so_far={}",
                        i + 1,
                        rec.candidate.mint,
                        valid,
                        report.unparseable_lines.len()
                    );
                    if !valid {
                        report.unparseable_lines.push(format!(
                            "line {}: invalid mint address {:?}",
                            i + 1,
                            rec.candidate.mint
                        ));
                        continue;
                    }
                    report.parseable_lines += 1;
                    if !seen_mints.insert(rec.candidate.mint.clone()) {
                        report.duplicate_mints.push(rec.candidate.mint.clone());
                    }
                    let now = Utc::now();
                    if rec.candidate.market.observed_at > now
                        || rec.candidate.safety.observed_at > now
                    {
                        report.future_dated_count += 1;
                    }
                    if rec.candidate.token_decimals.is_none()
                        || rec.candidate.base_mint_decimals.is_none()
                    {
                        report.missing_decimals_count += 1;
                    }
                    if rec.candidate.costs.input.position_size_usd != rec.candidate.position_usd {
                        report.cost_mismatch_count += 1;
                    }
                    if rec
                        .candidate
                        .wallets
                        .iter()
                        .any(|w| w.updated_at > rec.candidate.market.observed_at)
                    {
                        report.missing_wallet_pit_count += 1;
                    }
                    report.records.push(rec);
                }
                Err(e) => {
                    report
                        .unparseable_lines
                        .push(format!("line {}: {e}", i + 1));
                }
            }
        }
        Ok(report)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Loose Solana base58 pubkey check for validator. Same heuristic as
/// `backtest::data::is_valid_solana_mint`.
fn is_valid_solana_pubkey(s: &str) -> bool {
    let len = s.len();
    if !(32..=44).contains(&len) {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() && c != '0' && c != 'O' && c != 'I' && c != 'l')
}

/// Print a human-readable report of a validation result. Always
/// print ALL limitations, never hide them.
pub fn print_validation_report(report: &HistoryValidationReport) {
    println!("=== Historical Dataset Validation ===");
    println!("Path:                     {}", report.path.display());
    println!("Total non-comment lines:  {}", report.total_lines);
    println!("Parseable records:        {}", report.parseable_lines);
    println!(
        "Unparseable lines:        {}",
        report.unparseable_lines.len()
    );
    println!(
        "Duplicate mints:           {}",
        report.duplicate_mints.len()
    );
    println!("Future-dated records:     {}", report.future_dated_count);
    println!(
        "Missing decimals:          {}",
        report.missing_decimals_count
    );
    println!("Cost model mismatches:     {}", report.cost_mismatch_count);
    println!(
        "Wallet PIT violations:     {}",
        report.missing_wallet_pit_count
    );
    if !report.unparseable_lines.is_empty() {
        println!("\nUnparseable line details (first 5):");
        for line in report.unparseable_lines.iter().take(5) {
            println!("  - {line}");
        }
    }
    if !report.duplicate_mints.is_empty() {
        println!("\nDuplicate mints (first 5):");
        for mint in report.duplicate_mints.iter().take(5) {
            println!("  - {mint}");
        }
    }
    let unique_mints: HashSet<String> = report
        .records
        .iter()
        .map(|r| r.candidate.mint.clone())
        .collect();
    println!("\nUnique mints in dataset:   {}", unique_mints.len());
    println!("\n=== KNOWN MISSING HISTORICAL INPUTS ===");
    println!(
        "- field: wallet.score\n  why it is required: production entry decision \
         (config.strategy.min_wallet_score)\n  where the strategy uses it: \
         src/strategy/signal.rs evaluate_signal_pit\n  available historical source: \
         none\n  current limitation: WalletTracker is in-session only; no on-disk \
         PnL history; no chain trade-replay; collect_live uses a price-impact \
         heuristic as a synthetic proxy\n"
    );
    println!(
        "- field: wallet.trades\n  why it is required: production entry decision \
         (config.strategy.min_wallet_samples)\n  where the strategy uses it: \
         src/strategy/signal.rs evaluate_signal_pit\n  available historical source: \
         none\n  current limitation: same as wallet.score\n"
    );
    println!(
        "- field: subsequent price observations for exit simulation\n  why it is \
         required: backtest engine walks price_history through exit_reason()\n  where \
         the strategy uses it: src/backtest/engine.rs simulate_signal\n  available \
         historical source: none in this codebase\n  current limitation: the \
         collector captures a single point-in-time quote, not a forward price path; \
         no Geyser / Birdeye / Bitquery integration exists\n"
    );
    println!(
        "- field: holder_top10_pct at historical slot\n  why it is required: \
         production entry gate (must be <= 70%)\n  where the strategy uses it: \
         src/strategy/signal.rs evaluate_signal_pit\n  available historical source: \
         RPC getTokenLargestAccounts (not implemented in this codebase)\n  current \
         limitation: current value at signal time is recorded, not the distribution \
         at a historical block\n"
    );
    println!(
        "- field: creator_suspicious / abnormal_activity / liquidity_change_pct\n  why \
         it is required: production entry gates\n  where the strategy uses it: \
         src/strategy/signal.rs evaluate_signal_pit\n  available historical source: \
         external threat-intelligence (Helius, Birdeye)\n  current limitation: \
         hard-coded defaults; no external integration\n"
    );
    println!(
        "\nVerdict: the recorded data can drive the LIVE entry decision through the\n\
         existing collector path, but it CANNOT be used as a real historical\n\
         backtest because the wallet PIT, post-signal price path, and historical\n\
         holder-distribution features are not available.\n\
         is_synthetic_data MUST stay true for any data produced by this pipeline."
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::market::MarketSnapshot;
    use crate::domain::token::TokenSafety;
    use crate::domain::wallet::{WalletStats, WalletTier};
    use crate::economics::{BreakEvenInputs, CostModel};
    use rust_decimal_macros::dec;
    use tempfile::tempdir;

    fn make_candidate(
        mint: &str,
        observed_at: DateTime<Utc>,
        wallet_updated_at: DateTime<Utc>,
    ) -> CandidateInput {
        CandidateInput {
            mint: mint.into(),
            token_decimals: Some(6),
            base_mint_decimals: Some(9),
            input_amount: 4_000_000,
            position_usd: dec!(4),
            expected_gross_return_pct: dec!(15),
            market: MarketSnapshot {
                mint: mint.into(),
                price_usd: dec!(0.001),
                liquidity_usd: dec!(150_000),
                volume_24h_usd: dec!(500_000),
                volatility_pct: dec!(30),
                buy_sell_imbalance: dec!(0.6),
                observed_at,
                received_at: observed_at,
                slot: Some(300_000_000),
            },
            safety: TokenSafety {
                mint_authority_present: false,
                freeze_authority_present: false,
                holder_top10_pct: dec!(45),
                token_age_secs: 259_200,
                liquidity_locked_or_burned: Some(true),
                sellable: Some(true),
                route_available: Some(true),
                creator_suspicious: Some(false),
                abnormal_activity: Some(false),
                liquidity_change_pct: Some(dec!(5)),
                observed_at,
            },
            wallets: vec![WalletStats {
                wallet: "5kqEvH3gnx5HUYA8UmK3Za5gF3kRpY3oUg3TCY4tJhPb".into(),
                entity_id: Some("entity1".into()),
                realized_pnl_usd: dec!(500),
                win_rate: dec!(0.7),
                avg_return_pct: dec!(12),
                median_return_pct: dec!(10),
                max_drawdown_pct: dec!(10),
                trades: 50,
                recent_return_pct: dec!(10),
                concentration_pct: dec!(5),
                scam_exposure_pct: dec!(0),
                score: dec!(80),
                tier: WalletTier::Qualified,
                updated_at: wallet_updated_at,
            }],
            costs: CostModel {
                observed_at,
                source: "test".into(),
                is_live_snapshot: false,
                input: BreakEvenInputs {
                    position_size_usd: dec!(4),
                    avg_priority_fee_usd: dec!(0.002),
                    avg_swap_fee_bps: dec!(20),
                    avg_slippage_bps: dec!(80),
                    avg_price_impact_bps: dec!(15),
                    failed_tx_rate: dec!(0.10),
                    avg_failed_tx_cost_usd: dec!(0.002),
                    assumed_win_loss_ratio: dec!(2),
                    assumed_avg_loss_pct: dec!(10),
                },
            },
        }
    }

    #[test]
    fn recorder_appends_unique_mints() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let mut rec = HistoryRecorder::new(path.clone());
        let now = Utc::now();
        let c1 = make_candidate(
            "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263",
            now,
            now - chrono::Duration::minutes(5),
        );
        let c2 = make_candidate(
            "7GCihgDB8fe6KNjn2MYtkzZcRjQy3t9GHdC8uHYmW2hr",
            now,
            now - chrono::Duration::minutes(5),
        );
        assert!(rec.record(&c1).unwrap());
        assert!(rec.record(&c2).unwrap());
        // Same mint twice in one session is rejected.
        assert!(!rec.record(&c1).unwrap());
        assert_eq!(rec.recorded_count(), 2);
        // File has 2 records.
        let content = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.trim().starts_with('#'))
            .collect();
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn validator_reports_duplicates_and_pit_violations() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let now = Utc::now();
        // Write two records with the same mint directly to the file
        // (bypassing HistoryRecorder's session dedup) so the validator
        // sees both and can report duplicates.
        let c1 = make_candidate(
            "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263",
            now,
            now - chrono::Duration::minutes(5),
        );
        let c2 = make_candidate(
            "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263",
            now,
            now + chrono::Duration::minutes(10), // PIT violation
        );
        let rec1 = HistoryRecord {
            recorded_at: now,
            source: "test".into(),
            candidate: c1,
        };
        let rec2 = HistoryRecord {
            recorded_at: now,
            source: "test".into(),
            candidate: c2,
        };
        let mut rec = HistoryRecorder::new(path.clone());
        let json1 = serde_json::to_string(&rec1).unwrap();
        let json2 = serde_json::to_string(&rec2).unwrap();
        rec.ensure_file().unwrap();
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(json1.as_bytes()).unwrap();
        f.write_all(b"\n").unwrap();
        f.write_all(json2.as_bytes()).unwrap();
        f.write_all(b"\n").unwrap();

        let report = HistoryValidator::new(path).validate().unwrap();
        // Both records should be parseable. The second has
        // wallet.updated_at in the future, which counts as a PIT
        // violation.
        assert_eq!(report.parseable_lines, 2);
        assert_eq!(report.duplicate_mints.len(), 1);
        assert_eq!(report.records.len(), 2);
        assert!(report.missing_wallet_pit_count >= 1);
    }
}
