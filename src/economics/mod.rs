pub mod cost_model;
pub mod viability;

pub use cost_model::{CostModel, EconomicGate, EconomicGateDecision};
pub use viability::{break_even_calculator, BreakEvenInputs, BreakEvenResult, ViabilityError};
