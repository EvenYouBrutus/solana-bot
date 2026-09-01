use crate::{config::types::StrategyConfig, domain::position::Position};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ExitReason {
    StopLoss,
    TakeProfit,
    TrailingStop,
    TimeLimit,
    LiquidityDeterioration,
    SignalInvalidated,
}
impl ExitReason {
    pub fn as_str(&self) -> &str {
        match self {
            Self::StopLoss => "stop_loss",
            Self::TakeProfit => "take_profit",
            Self::TrailingStop => "trailing_stop",
            Self::TimeLimit => "time_limit",
            Self::LiquidityDeterioration => "liquidity_deterioration",
            Self::SignalInvalidated => "signal_invalidated",
        }
    }
}
impl std::fmt::Display for ExitReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
pub fn exit_reason(
    p: &Position,
    price: Decimal,
    liquidity: Option<Decimal>,
    min_liquidity: Decimal,
    invalidated: bool,
    now: DateTime<Utc>,
    c: &StrategyConfig,
) -> Option<ExitReason> {
    // Missing liquidity evidence is treated as unhealthy: fail-closed.
    match liquidity {
        Some(liq) => {
            if liq < min_liquidity {
                return Some(ExitReason::LiquidityDeterioration);
            }
        }
        None => {
            return Some(ExitReason::LiquidityDeterioration);
        }
    }
    if invalidated {
        return Some(ExitReason::SignalInvalidated);
    }
    let r = (price - p.entry_price_usd) / p.entry_price_usd * Decimal::new(100, 0);
    if r <= -c.stop_loss_pct {
        return Some(ExitReason::StopLoss);
    }
    if r >= c.take_profit_pct {
        return Some(ExitReason::TakeProfit);
    }
    if price <= p.high_water_price_usd * (Decimal::ONE - c.trailing_stop_pct / Decimal::new(100, 0))
    {
        return Some(ExitReason::TrailingStop);
    }
    if (now - p.entry_time).num_minutes() >= c.max_holding_minutes {
        return Some(ExitReason::TimeLimit);
    }
    None
}
