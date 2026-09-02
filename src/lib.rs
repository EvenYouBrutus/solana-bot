//! Safety-critical, capital-aware primitives. Strategy and execution layers are
//! deliberately absent until the Phase 0 gate has been affirmatively reviewed.
pub mod backtest;
pub mod collector;
pub mod config;
pub mod data;
pub mod domain;
pub mod economics;
pub mod execution;
pub mod exit_monitor;
pub mod historical;
pub mod history;
pub mod observability;
pub mod portfolio;
pub mod report;
pub mod risk;
pub mod runtime;
pub mod smart_money;
pub mod storage;
pub mod strategy;
