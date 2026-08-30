pub mod exit;
pub mod signal;
pub use exit::{exit_reason, ExitReason};
pub use signal::{evaluate_signal, StrategyDecision};
