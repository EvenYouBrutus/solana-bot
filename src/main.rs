use anyhow::Context;
use clap::{Parser, Subcommand};
use solana_smart_money_bot::economics::{break_even_calculator, BreakEvenInputs};
use std::{fs, path::PathBuf};

#[derive(Parser)] #[command(name = "solana-bot", about = "Phase 0 economics gate")]
struct Cli { #[command(subcommand)] command: Command }
#[derive(Subcommand)] enum Command { Economics { #[arg(long)] input: PathBuf } }
fn main() -> anyhow::Result<()> { let cli = Cli::parse(); match cli.command { Command::Economics { input } => { let text = fs::read_to_string(&input).with_context(|| format!("read {}", input.display()))?; let value: BreakEvenInputs = toml::from_str(&text).context("parse economic input TOML")?; println!("{}", serde_json::to_string_pretty(&break_even_calculator(&value)?)?); } }; Ok(()) }
