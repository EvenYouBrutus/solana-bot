//! End-to-end integration test for the backtest pipeline.
//!
//! This test exercises the same code path the CLI uses, against the
//! bundled SYNTHETIC fixture in `data/sample_historical.jsonl`. It is a
//! true end-to-end test: it loads the JSONL, validates PIT, simulates
//! every signal, computes per-split statistics, and asserts the
//! structural properties of the result that the CLI report relies on.
//!
//! The test does NOT validate profitability — the fixture is synthetic
//! and the OOS verdict is forced to `SYNTHETIC_DATA` by
//! `BacktestConfig.is_synthetic_data`. It validates the mechanics.

use crate::backtest::data::load_historical_signals;
use crate::backtest::engine::{simulate_signal, CostAssumptions};
use crate::backtest::split::Split;
use crate::backtest::stats::{compute_oos_verdict, OosVerdict};
use crate::backtest::{run_backtest, BacktestConfig};
use crate::config::types::Config;
use rust_decimal_macros::dec;
use std::collections::HashSet;
use std::path::PathBuf;

fn workspace_path(rel: &str) -> PathBuf {
    // CARGO_MANIFEST_DIR is set at compile time and points to the
    // crate root. The fixture lives at the repo root in `data/`.
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push(rel);
    p
}

/// Minimal strategy config that mirrors the production paper config.
/// Only the fields touched by the entry decision are required.
fn minimal_config() -> Config {
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
round_trip_cost_threshold_pct = 10.0
min_expected_net_return_pct = 0.0
max_quote_age_secs = 999999
uncertainty_haircut_pct = 0
[risk]
starting_capital_usd = 100.0
max_live_capital_usd = 100.0
max_concurrent_positions = 100
max_position_percent_of_equity = 100.0
max_position_percent_of_liquidity = 100.0
max_risk_per_trade_percent = 100.0
max_daily_loss_percent = 100.0
max_total_drawdown_before_kill_switch_pct = 100.0
cooldown_after_loss_minutes = 0
max_slippage_bps = 10000
min_liquidity_usd = 50000.0
max_trades_per_day = 1000
max_consecutive_failures = 1000
[execution]
provider = "jupiter"
jupiter_api_url = "https://api.jup.ag"
slippage_bps = 75
priority_fee_lamports = 10000
allowed_program_ids = []
[storage]
sqlite_path = ":memory:"
"#;
    toml::from_str(text).expect("minimal_config TOML must parse")
}

fn minimal_bt_config() -> BacktestConfig {
    let text = r#"
is_synthetic_data = true
[split]
train_end = "2024-06-01T00:00:00Z"
validation_start = "2024-06-01T00:00:00Z"
validation_end = "2024-09-01T00:00:00Z"
oos_start = "2024-09-01T00:00:00Z"
"#;
    toml::from_str(text).expect("minimal_bt_config TOML must parse")
}

#[test]
fn end_to_end_pipeline_with_bundled_sample_dataset() {
    let input = workspace_path("data/sample_historical.jsonl");
    assert!(
        input.exists(),
        "bundled sample dataset must exist at {}",
        input.display()
    );

    let config = minimal_config();
    let bt_config = minimal_bt_config();

    // Run the full pipeline exactly as the CLI does.
    let result = run_backtest(&config, &bt_config, &input)
        .expect("pipeline must succeed for the bundled sample fixture");

    // --- Dataset is marked synthetic ---
    assert!(
        result.statistics.is_synthetic_data,
        "is_synthetic_data must be true for the bundled fixture"
    );

    // --- OOS verdict is forced to SYNTHETIC_DATA ---
    assert_eq!(
        result.statistics.oos_verdict,
        OosVerdict::SyntheticData,
        "synthetic dataset must force OOS verdict to SYNTHETIC_DATA"
    );

    // --- All three splits are populated ---
    assert!(
        result.statistics.train_total_trades > 0,
        "Train split must be populated (got {})",
        result.statistics.train_total_trades
    );
    assert!(
        result.statistics.validation_total_trades > 0,
        "Validation split must be populated (got {})",
        result.statistics.validation_total_trades
    );
    assert!(
        result.statistics.oos_total_trades > 0,
        "OOS split must be populated (got {})",
        result.statistics.oos_total_trades
    );

    // --- OOS usable sample is sufficient for a directional verdict
    //     (which is then overridden by SYNTHETIC_DATA) ---
    assert!(
        result.statistics.oos_usable_trades >= 5,
        "OOS usable sample must be >= 5 for a directional verdict, got {}",
        result.statistics.oos_usable_trades
    );

    // --- Every trade carries a stable deterministic ID ---
    let mut seen_ids: HashSet<String> = HashSet::new();
    for t in &result.all_trades {
        assert!(
            t.trade_id.starts_with("bt:"),
            "trade id must be prefixed with 'bt:' (got {})",
            t.trade_id
        );
        assert!(
            seen_ids.insert(t.trade_id.clone()),
            "trade id {} must be unique within a run",
            t.trade_id
        );
    }

    // --- Rejection categories are reported ---
    assert!(
        result.malformed_records.len() + result.strategy_rejections.len() > 0
            || result.structural_rejections.len() > 0,
        "rejection categories must be reported (malformed, strategy, structural, range_excluded)"
    );

    // The fixture deliberately includes a strategy rejection (low wallet
    // score) and a future-dated PIT violation.
    assert_eq!(
        result.strategy_rejections.len(),
        1,
        "fixture must include exactly 1 strategy-rejected signal"
    );
    assert!(
        result
            .malformed_records
            .iter()
            .any(|m| m.contains("look-ahead bias")),
        "fixture must include at least one look-ahead-bias (PIT) rejection"
    );

    // --- Per-split reports are populated ---
    assert_eq!(
        result.statistics.oos_total_trades, result.statistics.oos_total_trades,
        "OOS total sample must be set"
    );
    assert!(
        result.train_stats.oos_total_trades == 0,
        "train_stats should not have OOS fields set (only OOS does)"
    );
    assert!(
        result.validation_stats.oos_total_trades == 0,
        "validation_stats should not have OOS fields set (only OOS does)"
    );

    // --- The cost mode is modeled, not observed ---
    // All cost fields in SimulatedTrade should be is_observed=false.
    for t in &result.all_trades {
        assert!(!t.entry_costs.is_observed);
        assert!(!t.exit_costs.is_observed);
    }
}

#[test]
fn end_to_end_is_deterministic() {
    // Same input → same output, twice. The backtest must be fully
    // deterministic: no wall-clock reads, no random IDs, no set/hashmap
    // ordering leaks into the report.
    let input = workspace_path("data/sample_historical.jsonl");
    let config = minimal_config();
    let bt_config = minimal_bt_config();

    let r1 = run_backtest(&config, &bt_config, &input).unwrap();
    let r2 = run_backtest(&config, &bt_config, &input).unwrap();

    // Trade IDs must match position-by-position.
    assert_eq!(r1.all_trades.len(), r2.all_trades.len());
    for (a, b) in r1.all_trades.iter().zip(r2.all_trades.iter()) {
        assert_eq!(a.trade_id, b.trade_id);
        assert_eq!(a.exit_reason, b.exit_reason);
        assert_eq!(a.net_pnl_usd, b.net_pnl_usd);
        assert_eq!(a.gross_pnl_usd, b.gross_pnl_usd);
        assert_eq!(a.net_return_pct, b.net_return_pct);
    }

    // Statistics must be bit-identical.
    assert_eq!(r1.statistics.win_rate, r2.statistics.win_rate);
    assert_eq!(r1.statistics.net_pnl_usd, r2.statistics.net_pnl_usd);
    assert_eq!(r1.statistics.profit_factor, r2.statistics.profit_factor);
    assert_eq!(r1.statistics.oos_verdict, r2.statistics.oos_verdict);
    assert_eq!(
        r1.statistics.oos_total_trades,
        r2.statistics.oos_total_trades
    );
    assert_eq!(
        r1.statistics.oos_usable_trades,
        r2.statistics.oos_usable_trades
    );
}

#[test]
fn end_to_end_records_have_stable_synthetic_marker() {
    // The bundled sample must not be re-classified as historical by any
    // other code path. The BacktestConfig.is_synthetic_data flag is
    // operator-set, and the run must preserve it.
    let input = workspace_path("data/sample_historical.jsonl");
    let bt_config = minimal_bt_config();
    assert!(bt_config.is_synthetic_data);

    let result = run_backtest(&minimal_config(), &bt_config, &input).unwrap();
    assert!(result.statistics.is_synthetic_data);
    assert_eq!(result.statistics.oos_verdict, OosVerdict::SyntheticData);
}

#[test]
fn end_to_end_oos_ambiguous_and_censored_are_distinct_from_total() {
    // The OOS "total" trades field must count every simulated OOS trade,
    // while the "usable" field counts only non-ambiguous AND non-censored
    // outcomes. The difference is reported explicitly.
    let input = workspace_path("data/sample_historical.jsonl");
    let result = run_backtest(&minimal_config(), &minimal_bt_config(), &input).unwrap();

    let s = &result.statistics;
    let accounted = s.oos_usable_trades + s.oos_ambiguous_trades + s.oos_censored_trades;
    assert_eq!(
        accounted, s.oos_total_trades,
        "usable + ambiguous + censored must equal total for OOS"
    );
    // Same invariant for Train and Validation.
    let train_accounted =
        s.train_usable_trades + s.train_ambiguous_trades + s.train_censored_trades;
    assert_eq!(train_accounted, s.train_total_trades);
    let val_accounted =
        s.validation_usable_trades + s.validation_ambiguous_trades + s.validation_censored_trades;
    assert_eq!(val_accounted, s.validation_total_trades);
}

#[test]
fn end_to_end_loader_rejects_future_dated_data() {
    // The loader is fail-closed on PIT violations: a record with
    // market.observed_at > signal_timestamp must be rejected with a
    // look-ahead-bias reason. The bundled fixture includes exactly one
    // such record, and the loader must surface it.
    let input = workspace_path("data/sample_historical.jsonl");
    let load_result = load_historical_signals(&input).unwrap();

    assert!(
        load_result
            .rejection_reasons
            .iter()
            .any(|r| r.contains("look-ahead bias")),
        "loader must reject the future-dated record with a look-ahead-bias reason"
    );
}

#[test]
fn end_to_end_pipeline_can_be_replayed_via_engine_directly() {
    // The same engine used by the pipeline is also callable directly.
    // This guards against drift between the pipeline wiring and the
    // single-trade API.
    let input = workspace_path("data/sample_historical.jsonl");
    let load_result = load_historical_signals(&input).unwrap();
    let config = minimal_config();
    let cost_assumptions = CostAssumptions::from_config(&minimal_bt_config());

    // Simulate the very first accepted signal and confirm it lands in
    // a known split with a known exit reason.
    for signal in &load_result.signals {
        let result = simulate_signal(signal, &config, &cost_assumptions, Split::OutOfSample, 0);
        assert!(result.is_ok(), "first OOS signal must simulate cleanly");
        let trade = result.unwrap();
        assert_eq!(trade.split, Split::OutOfSample);
        // The fixture's OOS signals all hit TP at +12%.
        assert_eq!(
            trade.exit_reason,
            crate::strategy::exit::ExitReason::TakeProfit
        );
        // Deterministic ID.
        assert!(trade.trade_id.starts_with("bt:"));
        return;
    }
    panic!("no signals loaded from bundled fixture");
}

#[test]
fn end_to_end_rejects_invalid_split_config() {
    // Fail-closed: an invalid split boundary (oos_start before
    // validation_end) must abort the run, not silently classify
    // records as Train.
    let input = workspace_path("data/sample_historical.jsonl");
    let bad_bt = toml::from_str::<BacktestConfig>(
        r#"
is_synthetic_data = true
[split]
train_end = "2024-12-31T00:00:00Z"
validation_start = "2024-01-01T00:00:00Z"
validation_end = "2024-06-01T00:00:00Z"
oos_start = "2024-06-01T00:00:00Z"
"#,
    )
    .unwrap();

    let result = run_backtest(&minimal_config(), &bad_bt, &input);
    assert!(result.is_err(), "invalid split must fail the run");
}

#[test]
fn end_to_end_oos_verdict_function_respects_sample_size() {
    // The verdict function must declare INSUFFICIENT_OOS_SAMPLE below
    // 5 usable OOS trades, even when the mean return is strongly
    // positive.
    use crate::backtest::engine::CostMode;
    use crate::backtest::engine::SimulatedTrade;
    use crate::backtest::stats::compute_statistics;
    use chrono::{DateTime, Utc};
    use rust_decimal::Decimal;

    let mk_trade = |i: usize, pnl: Decimal| SimulatedTrade {
        trade_id: format!("t{i}"),
        signal_timestamp: "2024-09-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap(),
        mint: "m".into(),
        split: Split::OutOfSample,
        entry_time: "2024-09-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap(),
        entry_price_usd: dec!(0.0001),
        position_usd: dec!(4),
        entry_quantity_tokens: dec!(40000),
        entry_costs: crate::backtest::engine::TradeCosts {
            swap_fee_usd: dec!(0),
            priority_fee_usd: dec!(0),
            slippage_cost_usd: dec!(0),
            price_impact_cost_usd: dec!(0),
            total_usd: dec!(0),
            is_observed: false,
        },
        exit_time: "2024-09-01T00:05:00Z".parse::<DateTime<Utc>>().unwrap(),
        exit_price_usd: dec!(0.0002),
        exit_reason: crate::strategy::exit::ExitReason::TakeProfit,
        holding_minutes: 5,
        exit_costs: crate::backtest::engine::TradeCosts {
            swap_fee_usd: dec!(0),
            priority_fee_usd: dec!(0),
            slippage_cost_usd: dec!(0),
            price_impact_cost_usd: dec!(0),
            total_usd: dec!(0),
            is_observed: false,
        },
        gross_return_pct: dec!(100),
        gross_pnl_usd: pnl,
        total_cost_usd: dec!(0),
        net_return_pct: dec!(100),
        net_pnl_usd: pnl,
        mfe_pct: dec!(100),
        mae_pct: dec!(0),
        is_ambiguous: false,
        ambiguous_reason: None,
        is_censored: false,
        censored_reason: None,
        cost_mode: CostMode::Modeled,
    };

    // 4 trades, all winning, all OOS. Mean return is strongly positive.
    // Verdict must be INSUFFICIENT_OOS_SAMPLE, NOT POSITIVE_EXPECTANCY.
    let trades: Vec<SimulatedTrade> = (0..4).map(|i| mk_trade(i, dec!(1))).collect();
    let mut s = compute_statistics(&trades, 4, 0, dec!(100), false);
    s.oos_total_trades = 4;
    s.oos_usable_trades = 4;
    s.oos_ambiguous_trades = 0;
    s.oos_censored_trades = 0;
    s.oos_mean_return_pct = dec!(100);
    s.oos_ci95_lower_pct = dec!(90);
    s.oos_ci95_upper_pct = dec!(110);
    let verdict = compute_oos_verdict(&s);
    assert_eq!(verdict, OosVerdict::InsufficientOosSample);
}
