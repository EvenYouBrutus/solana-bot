//! Real historical market-data, wallet-activity, and token-safety ingestion
//! for the Solana backtest pipeline.
//!
//! Every artifact in this module is built from real external providers:
//! - OHLCV: a Solana market-data provider (Birdeye-compatible REST endpoint)
//!   configured with an API key from the environment.
//! - Wallet activity: Solana RPC `getSignaturesForAddress` +
//!   `getTransaction` (Helius or any Solana RPC that returns
//!   jsonParsed historical transactions).
//! - Token safety: Solana RPC `getAccountInfo` for the mint, point-in-time.
//!
//! Every reconstruction is point-in-time: the dataset carries the
//! timestamp of every observation, and the backtest engine refuses any
//! record where the decision-time data is from after the signal.
//!
//! This module is the missing piece between the synthetic fixtures in
//! `data/sample_historical.jsonl` and a real historical backtest.

pub mod build;
pub mod cost;
pub mod ohlcv;
pub mod safety;
pub mod signal;
pub mod validate;
pub mod wallet;

pub use build::{BuildOptions, BuildReport, HistoricalBuilder};
pub use ohlcv::{OhlcvCandle, OhlcvProvider, OhlcvRequest};
pub use safety::{HistoricalTokenSafety, SafetyProvider};
pub use signal::{HistoricalDatasetEntry, HistoricalSignalBuilder, SignalBuildError};
pub use validate::{HistoricalValidationReport, HistoricalValidator};
pub use wallet::{HistoricalWalletStats, WalletReconstructor};
