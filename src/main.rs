use anyhow::Context;
use clap::{Parser, Subcommand};
use solana_smart_money_bot::{
    config::{load, types::{Config, Mode}},
    data::rpc::RpcPool,
    economics::{break_even_calculator, BreakEvenInputs},
    execution::JupiterExecutor,
    observability,
    runtime::{self, SessionDeps},
    storage::StateStore,
};
use std::{fs, path::PathBuf, time::Duration};

#[derive(Parser)]
#[command(name = "solana-bot", about = "Fail-closed Solana smart-money trading system")]
struct Cli { #[command(subcommand)] command: Command }

#[derive(Subcommand)]
enum Command {
    /// Compute round-trip economics for a TOML scenario.
    Economics { #[arg(long)] input: PathBuf },
    /// Validate configuration, persistence, RPC, and execution health.
    Check { #[arg(long)] config: PathBuf },
    /// Run the trading session (paper or live).
    Run { #[arg(long)] config: PathBuf },
    /// Reconcile persisted orders/positions against the chain and exit.
    Reconcile { #[arg(long)] config: PathBuf },
    /// Sell every open position through the full execution pipeline.
    ExitAll { #[arg(long)] config: PathBuf },
    /// Latch the persistent emergency stop (manual exits stay possible).
    EmergencyStop { #[arg(long)] config: PathBuf, #[arg(long)] reason: Option<String> },
    /// Clear the persistent emergency stop after operator review.
    ClearEmergencyStop { #[arg(long)] config: PathBuf },
}

fn build_executor(c: &Config, rpc: RpcPool) -> anyhow::Result<JupiterExecutor> {
    if c.mode == Mode::Live {
        let name = c.execution.live_signer_env.as_deref().context("missing live signer env")?;
        std::env::var(name).context("live signer secret unavailable")?;
    }
    JupiterExecutor::new(
        c.execution.jupiter_api_url.clone(),
        rpc,
        c.execution.live_signer_env.clone(),
        c.economics.max_quote_age_secs,
        c.execution.allowed_program_ids.clone(),
        c.execution.priority_fee_lamports,
        c.execution.confirm_timeout_secs,
        c.execution.confirm_poll_ms,
    )
}

fn base_setup(path: &PathBuf) -> anyhow::Result<(Config, StateStore, RpcPool)> {
    let c = load(path)?;
    let rpc = RpcPool::with_attempts(c.rpc.http_endpoints.clone(), Duration::from_secs(c.rpc.request_timeout_secs), c.rpc.max_attempts)?;
    let state = StateStore::open(&c.storage.sqlite_path).context("persistence check")?;
    Ok((c, state, rpc))
}

async fn check(path: PathBuf) -> anyhow::Result<()> {
    let (c, state, rpc) = base_setup(&path)?;
    rpc.health().await.context("RPC health check")?;
    if state.kill_switch_reason()?.is_some() { anyhow::bail!("persisted kill switch is latched; operator review is required"); }
    if !state.incomplete_orders()?.is_empty() { anyhow::bail!("unreconciled orders exist; refusing new entries"); }
    if c.mode == Mode::Live {
        let name = c.execution.live_signer_env.as_deref().context("missing live signer env")?;
        let value = std::env::var(name).context("live signer secret unavailable")?;
        let _: Vec<u8> = serde_json::from_str(&value).context("live signer is not JSON byte-array keypair material")?;
    }
    let execution = build_executor(&c, rpc)?;
    execution.health().await.context("execution health check")?;
    println!("configuration, persistence, RPC, and execution health are valid; mode={:?}", c.mode);
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
    let execution = build_executor(&c, rpc)?;
    execution.health().await.context("execution health check")?;
    if c.mode == Mode::Live {
        tracing::warn!(cap = %c.risk.max_live_capital_usd, "live mode explicitly armed");
    } else {
        tracing::info!("paper mode: identical strategy/risk/pipeline, no transactions submitted");
    }
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = tx.send(());
    });
    let deps = SessionDeps { config: &c, store: &state, executor: &execution, rpc: &RpcPool::with_attempts(vec!["http://127.0.0.1:1".into()], Duration::from_secs(2), 1)? };
    runtime::run_session(deps, rx).await
}

async fn reconcile(path: PathBuf) -> anyhow::Result<()> {
    let (c, state, rpc) = base_setup(&path)?;
    observability::init(c.observability.log_format == "json");
    rpc.health().await.context("RPC health check")?;
    let execution = build_executor(&c, rpc.clone())?;
    let deps = SessionDeps { config: &c, store: &state, executor: &execution, rpc: &rpc };
    let summary = runtime::run_reconciliation(deps).await?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    if summary.unresolved_orders > 0 || summary.onchain_errors > 0 { anyhow::bail!("reconciliation incomplete; operator review required"); }
    Ok(())
}

async fn exit_all(path: PathBuf) -> anyhow::Result<()> {
    let (c, state, rpc) = base_setup(&path)?;
    observability::init(c.observability.log_format == "json");
    rpc.health().await.context("RPC health check")?;
    let execution = build_executor(&c, rpc.clone())?;
    let deps = SessionDeps { config: &c, store: &state, executor: &execution, rpc: &rpc };
    let summary = runtime::run_reconciliation(&deps).await?;
    if summary.unresolved_orders > 0 { anyhow::bail!("unresolved orders remain; run Reconcile and review before exiting"); }
    let exited = runtime::exit_all_positions(&deps).await?;
    println!("manual exit attempted for {exited} position(s)");
    Ok(())
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Economics { input } => {
            let text = fs::read_to_string(&input).with_context(|| format!("read {}", input.display()))?;
            let value: BreakEvenInputs = toml::from_str(&text).context("parse economic input TOML")?;
            println!("{}", serde_json::to_string_pretty(&break_even_calculator(&value)?)?);
        }
        Command::Check { config } => check(config).await?,
        Command::Run { config } => run(config).await?,
        Command::Reconcile { config } => reconcile(config).await?,
        Command::ExitAll { config } => exit_all(config).await?,
        Command::EmergencyStop { config, reason } => emergency_stop(config, reason)?,
        Command::ClearEmergencyStop { config } => clear_emergency_stop(config)?,
    }
    Ok(())
}
