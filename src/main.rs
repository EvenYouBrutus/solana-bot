use anyhow::Context;
use clap::{Parser, Subcommand};
use solana_smart_money_bot::{
    backtest::{self, BacktestConfig},
    collector::{self, CandidateCollector},
    config::{
        load,
        types::{Config, Mode},
    },
    data::rpc::RpcPool,
    economics::{break_even_calculator, BreakEvenInputs},
    execution::{DeterministicExecutor, Executor, JupiterExecutor, PaperExecutor},
    history, observability,
    report::PerformanceReport,
    runtime::{self, SessionDeps},
    storage::StateStore,
};
use std::{fs, path::PathBuf, sync::Arc, time::Duration};

#[derive(Parser)]
#[command(
    name = "solana-bot",
    about = "Fail-closed Solana smart-money trading system"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Compute round-trip economics for a TOML scenario.
    Economics {
        #[arg(long)]
        input: PathBuf,
    },
    /// Validate configuration, persistence, RPC, and execution health.
    Check {
        #[arg(long)]
        config: PathBuf,
    },
    /// Run the trading session (paper or live).
    Run {
        #[arg(long)]
        config: PathBuf,
    },
    /// Reconcile persisted orders/positions against the chain and exit.
    Reconcile {
        #[arg(long)]
        config: PathBuf,
    },
    /// Sell every open position through the full execution pipeline.
    ExitAll {
        #[arg(long)]
        config: PathBuf,
    },
    /// Latch the persistent emergency stop (manual exits stay possible).
    EmergencyStop {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Clear the persistent emergency stop after operator review.
    ClearEmergencyStop {
        #[arg(long)]
        config: PathBuf,
    },
    /// Generate a performance report from persisted trade data.
    Report {
        #[arg(long)]
        config: PathBuf,
        /// Output format: "text" (default) or "json"
        #[arg(long, default_value = "text")]
        format: String,
    },
    /// Run historical backtest/replay with OOS split.
    Backtest {
        #[arg(long)]
        config: PathBuf,
        /// Path to backtest-specific TOML (split boundaries, cost assumptions)
        #[arg(long, default_value = "config/backtest.toml")]
        backtest_config: PathBuf,
        /// Path to JSONL historical signal data
        #[arg(long)]
        input: PathBuf,
        /// Output format: "text" (default) or "json"
        #[arg(long, default_value = "text")]
        format: String,
        /// Optional path to write trade-by-trade JSON output
        #[arg(long)]
        trades: Option<PathBuf>,
    },
    /// Collect real historical observations by polling the live
    /// Jupiter quote path. Appends `CandidateInput` records to a
    /// JSONL file. Each record carries a real on-chain market snapshot,
    /// route-availability check, and a calibrated cost model.
    ///
    /// This is NOT a full historical reconstruction. See
    /// `docs/HISTORICAL_DATA_PIPELINE.md` for the full list of
    /// MISSING historical inputs. Use this only to produce
    /// observation recordings for a synthetic-fixture integration
    /// test, never as evidence of real strategy performance.
    CollectHistory {
        #[arg(long)]
        config: PathBuf,
        /// Output JSONL path.
        #[arg(long, default_value = "data/historical_candidates.jsonl")]
        output: PathBuf,
        /// Number of polling iterations. Each iteration is one call to
        /// the existing live collector. With the rate-limited backoff,
        /// each iteration is a real HTTP call to Jupiter.
        #[arg(long, default_value = "1")]
        iterations: u32,
    },
    /// Validate a JSONL historical dataset and print a report of its
    /// structural soundness, PIT violations, and known missing inputs.
    /// Does NOT fabricate or relax any constraint. A dataset that
    /// cannot honestly back the strategy is reported as unfit.
    ValidateHistory {
        /// Path to the JSONL dataset (the file produced by
        /// `collect-history` or any other pipeline).
        #[arg(long)]
        input: PathBuf,
    },
}

fn build_executor(c: &Config, rpc: RpcPool) -> anyhow::Result<Arc<dyn Executor>> {
    match c.mode {
        Mode::Live => {
            let name = c
                .execution
                .live_signer_env
                .as_deref()
                .context("missing live signer env")?;
            std::env::var(name).context("live signer secret unavailable")?;
            let jup = JupiterExecutor::new(
                c.execution.jupiter_api_url.clone(),
                rpc,
                c.execution.live_signer_env.clone(),
                c.execution.jupiter_api_key_env.clone(),
                c.economics.max_quote_age_secs,
                c.execution.max_fee_lamports,
                c.execution.allowed_program_ids.clone(),
                c.execution.priority_fee_lamports,
                c.execution.confirm_timeout_secs,
                c.execution.confirm_poll_ms,
            )?;
            Ok(Arc::new(jup))
        }
        Mode::Paper => {
            let jup = JupiterExecutor::new(
                c.execution.jupiter_api_url.clone(),
                rpc,
                None,
                None,
                c.economics.max_quote_age_secs,
                c.execution.max_fee_lamports,
                c.execution.allowed_program_ids.clone(),
                c.execution.priority_fee_lamports,
                c.execution.confirm_timeout_secs,
                c.execution.confirm_poll_ms,
            )?;
            Ok(Arc::new(PaperExecutor::new(
                jup,
                c.runtime.paper_fill_haircut_bps,
            )))
        }
        Mode::Replay => {
            // Replay mode uses a deterministic executor that serves quotes
            // from the candidate dataset. No network calls are made.
            Ok(Arc::new(PaperExecutor::new(
                DeterministicExecutor::new(),
                c.runtime.paper_fill_haircut_bps,
            )))
        }
    }
}

fn base_setup(path: &PathBuf) -> anyhow::Result<(Config, StateStore, RpcPool)> {
    let c = load(path)?;
    let rpc = RpcPool::with_attempts(
        c.rpc.http_endpoints.clone(),
        Duration::from_secs(c.rpc.request_timeout_secs),
        c.rpc.max_attempts,
    )?;
    let state = StateStore::open(&c.storage.sqlite_path).context("persistence check")?;
    Ok((c, state, rpc))
}

async fn check(path: PathBuf) -> anyhow::Result<()> {
    let (c, state, rpc) = base_setup(&path)?;
    rpc.health().await.context("RPC health check")?;
    if state.kill_switch_reason()?.is_some() {
        anyhow::bail!("persisted kill switch is latched; operator review is required");
    }
    if !state.incomplete_orders()?.is_empty() {
        anyhow::bail!("unreconciled orders exist; refusing new entries");
    }
    if c.mode == Mode::Live {
        let name = c
            .execution
            .live_signer_env
            .as_deref()
            .context("missing live signer env")?;
        let value = std::env::var(name).context("live signer secret unavailable")?;
        let _: Vec<u8> = serde_json::from_str(&value)
            .context("live signer is not JSON byte-array keypair material")?;
    }
    let executor = build_executor(&c, rpc)?;
    executor.health().await.context("execution health check")?;
    println!(
        "configuration, persistence, RPC, and execution health are valid; mode={:?}",
        c.mode
    );
    Ok(())
}

async fn run(path: PathBuf) -> anyhow::Result<()> {
    let (c, state, rpc) = base_setup(&path)?;
    observability::init(c.observability.log_format == "json");
    if let Some(reason) = state.kill_switch_reason()? {
        tracing::warn!(%reason, "persisted kill switch latched; entries stay disabled for this session");
    }
    if let Some(reason) = state.emergency_stop()? {
        tracing::warn!(%reason, "persisted emergency stop active; entries stay disabled, manual exits available");
    }
    rpc.health().await.context("RPC health check")?;
    let executor = build_executor(&c, rpc.clone())?;
    executor.health().await.context("execution health check")?;
    if c.mode == Mode::Live {
        tracing::warn!(cap = %c.risk.max_live_capital_usd, "live mode explicitly armed");
    } else {
        tracing::info!(mode = ?c.mode, "paper/replay mode: no transactions submitted");
    }
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = tx.send(());
    });
    let config = Arc::new(c);
    let store = Arc::new(state);
    let rpc = Arc::new(rpc);
    let deps = session_deps(config, store, executor, rpc);
    runtime::run_session(deps, rx).await
}

async fn reconcile(path: PathBuf) -> anyhow::Result<()> {
    let (c, state, rpc) = base_setup(&path)?;
    observability::init(c.observability.log_format == "json");
    rpc.health().await.context("RPC health check")?;
    let executor = build_executor(&c, rpc.clone())?;
    let config = Arc::new(c);
    let store = Arc::new(state);
    let rpc = Arc::new(rpc);
    let deps = session_deps(config, store, executor, rpc);
    let summary = runtime::run_reconciliation(&deps).await?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    if summary.unresolved_orders > 0 || summary.onchain_errors > 0 {
        anyhow::bail!("reconciliation incomplete; operator review required");
    }
    Ok(())
}

async fn exit_all(path: PathBuf) -> anyhow::Result<()> {
    let (c, state, rpc) = base_setup(&path)?;
    observability::init(c.observability.log_format == "json");
    rpc.health().await.context("RPC health check")?;
    let executor = build_executor(&c, rpc.clone())?;
    let config = Arc::new(c);
    let store = Arc::new(state);
    let rpc = Arc::new(rpc);
    let deps = session_deps(config, store, executor, rpc);
    let summary = runtime::run_reconciliation(&deps).await?;
    if summary.unresolved_orders > 0 {
        anyhow::bail!("unresolved orders remain; run Reconcile and review before exiting");
    }
    let exited = runtime::exit_all_positions(&deps).await?;
    println!("manual exit attempted for {exited} position(s)");
    Ok(())
}

fn session_deps(
    config: Arc<Config>,
    store: Arc<StateStore>,
    executor: Arc<dyn Executor>,
    rpc: Arc<RpcPool>,
) -> SessionDeps {
    SessionDeps {
        config,
        store,
        executor,
        rpc,
    }
}

fn emergency_stop(path: PathBuf, reason: Option<String>) -> anyhow::Result<()> {
    let (_c, state, _rpc) = base_setup(&path)?;
    state.set_emergency_stop(reason.as_deref().unwrap_or("operator halt"))?;
    println!("emergency stop latched; new trades blocked, manual exits remain available");
    Ok(())
}

fn clear_emergency_stop(path: PathBuf) -> anyhow::Result<()> {
    let (_c, state, _rpc) = base_setup(&path)?;
    state.clear_emergency_stop()?;
    println!("emergency stop cleared");
    Ok(())
}

fn report(path: PathBuf, format: &str) -> anyhow::Result<()> {
    let (c, state, _rpc) = base_setup(&path)?;
    let report = PerformanceReport::generate(&state, c.risk.starting_capital_usd)?;
    match format {
        "json" => {
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        _ => {
            println!("{report}");
        }
    }
    Ok(())
}

fn backtest_cmd(
    config_path: PathBuf,
    bt_config_path: PathBuf,
    input: PathBuf,
    format: &str,
    trades_path: Option<PathBuf>,
) -> anyhow::Result<()> {
    let (config, _state, _rpc) = base_setup(&config_path)?;
    let bt_toml = std::fs::read_to_string(&bt_config_path)
        .with_context(|| format!("read {}", bt_config_path.display()))?;
    let bt_config: BacktestConfig = toml::from_str(&bt_toml).context("parse backtest config")?;
    let output =
        backtest::run_backtest(&config, &bt_config, &input).map_err(|e| anyhow::anyhow!(e))?;
    match format {
        "json" => {
            println!(
                "{}",
                output.to_json_summary().map_err(|e| anyhow::anyhow!(e))?
            );
        }
        _ => {
            println!("{output}");
        }
    }
    if let Some(trades_path) = trades_path {
        let json = serde_json::to_string_pretty(&output.all_trades)?;
        std::fs::write(&trades_path, json)
            .with_context(|| format!("write {}", trades_path.display()))?;
        println!("trade-by-trade output written to {}", trades_path.display());
    }
    Ok(())
}

async fn collect_history_cmd(
    config_path: PathBuf,
    output: PathBuf,
    iterations: u32,
) -> anyhow::Result<()> {
    eprintln!(
        "WARNING: collect-history records live observations into a JSONL file.\n\
         This is a real-data recording pipeline, NOT a historical reconstruction.\n\
         See docs/HISTORICAL_DATA_PIPELINE.md for the full list of fields that\n\
         cannot be reconstructed from the current RPC pool (wallet PnL, post-signal\n\
         price path, historical holder distribution, threat-intel features).\n\
         The recorded dataset CANNOT be used for a real backtest with\n\
         is_synthetic_data = false."
    );
    let (c, _state, rpc) = base_setup(&config_path)?;
    observability::init(c.observability.log_format == "json");
    rpc.health().await.context("RPC health check")?;
    let executor = build_executor(&c, rpc.clone())?;
    executor.health().await.context("execution health check")?;
    let mut recorder = history::HistoryRecorder::new(output.clone());
    let mut collector = CandidateCollector::new();
    for i in 0..iterations {
        match collector.collect_live(&c, &executor).await {
            collector::CollectResult::Candidates(cands) => {
                for c in &cands {
                    match recorder.record(c) {
                        Ok(true) => {
                            tracing::info!(iteration = i, mint = %c.mint, "recorded");
                        }
                        Ok(false) => {
                            tracing::debug!(
                                iteration = i,
                                mint = %c.mint,
                                "skipped (duplicate or empty mint)"
                            );
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "failed to append to history file");
                        }
                    }
                }
            }
            collector::CollectResult::RateLimited { backoff_secs } => {
                tracing::warn!(backoff_secs, "rate limited, sleeping");
                tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
            }
            collector::CollectResult::Transient(msg) => {
                tracing::warn!(reason = %msg, "transient collection error");
            }
        }
    }
    println!(
        "wrote {} records to {}",
        recorder.recorded_count(),
        output.display()
    );
    Ok(())
}

fn validate_history_cmd(input: PathBuf) -> anyhow::Result<()> {
    let validator = history::HistoryValidator::new(input);
    let report = validator
        .validate()
        .map_err(|e| anyhow::anyhow!("validate {}: {e}", validator.path().display()))?;
    history::print_validation_report(&report);
    if !report.unparseable_lines.is_empty() || !report.duplicate_mints.is_empty() {
        anyhow::bail!(
            "dataset has {} unparseable line(s) and {} duplicate mint(s); refusing to certify",
            report.unparseable_lines.len(),
            report.duplicate_mints.len()
        );
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Economics { input } => {
            let text =
                fs::read_to_string(&input).with_context(|| format!("read {}", input.display()))?;
            let value: BreakEvenInputs =
                toml::from_str(&text).context("parse economic input TOML")?;
            println!(
                "{}",
                serde_json::to_string_pretty(&break_even_calculator(&value)?)?
            );
        }
        Command::Check { config } => check(config).await?,
        Command::Run { config } => run(config).await?,
        Command::Reconcile { config } => reconcile(config).await?,
        Command::ExitAll { config } => exit_all(config).await?,
        Command::EmergencyStop { config, reason } => emergency_stop(config, reason)?,
        Command::ClearEmergencyStop { config } => clear_emergency_stop(config)?,
        Command::Report { config, format } => report(config, &format)?,
        Command::Backtest {
            config,
            backtest_config,
            input,
            format,
            trades,
        } => backtest_cmd(config, backtest_config, input, &format, trades)?,
        Command::CollectHistory {
            config,
            output,
            iterations,
        } => collect_history_cmd(config, output, iterations).await?,
        Command::ValidateHistory { input } => validate_history_cmd(input)?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_smart_money_bot::config::types::Config;

    fn sample_config() -> Config {
        let text = r#"
mode = "paper"
[rpc]
http_endpoints = ["https://configured-rpc.example"]
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
max_live_capital_usd = 25.0
max_concurrent_positions = 1
max_position_percent_of_equity = 5.0
max_position_percent_of_liquidity = 0.10
max_risk_per_trade_percent = 0.5
max_daily_loss_percent = 2.0
max_total_drawdown_before_kill_switch_pct = 5.0
cooldown_after_loss_minutes = 30
max_slippage_bps = 100
min_liquidity_usd = 50000.0
max_trades_per_day = 3
[execution]
provider = "jupiter"
jupiter_api_url = "https://api.jup.ag"
slippage_bps = 75
priority_fee_lamports = 10000
allowed_program_ids = []
[storage]
sqlite_path = ":memory:"
"#;
        toml::from_str(text).expect("valid test config")
    }

    #[test]
    fn session_deps_bind_configured_rpc_not_loopback_placeholder() {
        let config = sample_config();
        let store = StateStore::open(":memory:").unwrap();
        let rpc =
            RpcPool::with_attempts(config.rpc.http_endpoints.clone(), Duration::from_secs(2), 1)
                .unwrap();
        let placeholder =
            RpcPool::with_attempts(vec!["http://127.0.0.1:1".into()], Duration::from_secs(2), 1)
                .unwrap();
        let executor = build_executor(&config, rpc.clone()).unwrap();
        let config = Arc::new(config);
        let store = Arc::new(store);
        let rpc = Arc::new(rpc);
        let deps = session_deps(config, store, executor, rpc.clone());
        assert!(
            std::ptr::eq(deps.rpc.as_ref(), rpc.as_ref()),
            "session must use the configured RPC pool from base_setup"
        );
        assert!(
            !std::ptr::eq(deps.rpc.as_ref(), &placeholder),
            "session must not substitute a loopback placeholder RPC"
        );
        assert_eq!(deps.rpc.endpoints(), rpc.endpoints());
        assert!(!deps
            .rpc
            .endpoints()
            .iter()
            .any(|endpoint| endpoint.contains("127.0.0.1:1")));
    }

    #[test]
    fn paper_mode_builds_non_live_executor() {
        let mut config = sample_config();
        config.mode = Mode::Paper;
        let rpc =
            RpcPool::with_attempts(config.rpc.http_endpoints.clone(), Duration::from_secs(2), 1)
                .unwrap();
        let executor = build_executor(&config, rpc).unwrap();
        assert!(!executor.is_live());
        assert!(executor.signer_pubkey().is_none());
    }

    #[test]
    fn replay_mode_builds_non_live_executor() {
        let mut config = sample_config();
        config.mode = Mode::Replay;
        let rpc =
            RpcPool::with_attempts(config.rpc.http_endpoints.clone(), Duration::from_secs(2), 1)
                .unwrap();
        let executor = build_executor(&config, rpc).unwrap();
        assert!(!executor.is_live());
        assert!(executor.signer_pubkey().is_none());
    }

    #[test]
    fn live_mode_requires_signer_env() {
        let mut config = sample_config();
        config.mode = Mode::Live;
        config.execution.live_signer_env = None;
        let rpc =
            RpcPool::with_attempts(config.rpc.http_endpoints.clone(), Duration::from_secs(2), 1)
                .unwrap();
        assert!(
            build_executor(&config, rpc).is_err(),
            "live mode without signer env must fail"
        );
    }

    #[test]
    fn paper_mode_ignores_signer_env_in_config() {
        let mut config = sample_config();
        config.mode = Mode::Paper;
        config.execution.live_signer_env = Some("SOLANA_BOT_KEYPAIR_JSON".into());
        let rpc =
            RpcPool::with_attempts(config.rpc.http_endpoints.clone(), Duration::from_secs(2), 1)
                .unwrap();
        let executor = build_executor(&config, rpc).unwrap();
        assert!(!executor.is_live());
        assert!(executor.signer_pubkey().is_none());
    }
}
