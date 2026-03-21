use crate::config::RiskConfig;
use crate::types::{BotState, Outcome};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::fmt;
use std::sync::Arc;
use tracing::{info, warn};

// ── Sizing Result ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SizingResult {
    pub size: Decimal,
    pub allowed: bool,
    pub reason: String,
}

impl SizingResult {
    fn reject(reason: impl Into<String>) -> Self {
        Self {
            size: Decimal::ZERO,
            allowed: false,
            reason: reason.into(),
        }
    }

    fn allow(size: Decimal) -> Self {
        Self {
            size,
            allowed: true,
            reason: "ok".into(),
        }
    }
}

// ── Risk Metrics ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RiskMetrics {
    pub daily_pnl: Decimal,
    pub inventory_up: Decimal,
    pub inventory_down: Decimal,
    pub inventory_ratio: Decimal,
    pub consecutive_losses: u64,
    pub is_paused: bool,
    pub active_orders: usize,
}

impl fmt::Display for RiskMetrics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "PnL=${:.2} | UP={:.1} DOWN={:.1} ratio={:.2} | losses={} paused={} orders={}",
            self.daily_pnl,
            self.inventory_up,
            self.inventory_down,
            self.inventory_ratio,
            self.consecutive_losses,
            self.is_paused,
            self.active_orders
        )
    }
}

// ── Risk Engine ───────────────────────────────────────────────────────────────

pub struct RiskEngine {
    config: RiskConfig,
    state: Arc<BotState>,
}

impl RiskEngine {
    pub fn new(config: RiskConfig, state: Arc<BotState>) -> Self {
        Self { config, state }
    }

    /// Check if trading is allowed; returns SizingResult.
    pub fn can_trade(&self) -> SizingResult {
        if self.state.is_paused() {
            return SizingResult::reject("Bot is paused (circuit breaker)");
        }

        let consecutive = self.state.consecutive_losses();
        if consecutive >= self.config.circuit_breaker_losses {
            self.state.pause();
            warn!(
                "Circuit breaker triggered: {} consecutive losses",
                consecutive
            );
            return SizingResult::reject(format!(
                "Circuit breaker: {} consecutive losses",
                consecutive
            ));
        }

        let daily_pnl = self.state.get_daily_pnl();
        let loss_limit =
            -(Decimal::try_from(self.config.bankroll).unwrap_or(dec!(1000))
                * Decimal::try_from(self.config.daily_loss_limit_pct).unwrap_or(dec!(0.05)));
        if daily_pnl < loss_limit {
            return SizingResult::reject(format!(
                "Daily loss limit reached: ${:.2}",
                daily_pnl
            ));
        }

        SizingResult::allow(Decimal::ZERO)
    }

    /// Half-Kelly position sizing.
    pub fn calculate_size(&self, edge: Decimal, odds: Decimal, outcome: Outcome) -> SizingResult {
        let gate = self.can_trade();
        if !gate.allowed {
            return gate;
        }

        if odds.is_zero() {
            return SizingResult::reject("Zero odds");
        }

        // Half Kelly fraction
        let kelly_full = edge / odds;
        let kelly_half = kelly_full / dec!(2);
        let clamped_kelly = kelly_half.max(dec!(0)).min(dec!(0.05));

        let bankroll = Decimal::try_from(self.config.bankroll).unwrap_or(dec!(1000));
        let max_exposure = bankroll
            * Decimal::try_from(self.config.max_exposure_pct).unwrap_or(dec!(0.10));

        let raw_size = bankroll * clamped_kelly;
        let size = raw_size.min(max_exposure);

        // Check inventory ratio
        let ratio = self.state.inventory_ratio();
        let max_ratio =
            Decimal::try_from(self.config.max_inventory_ratio).unwrap_or(dec!(0.75));

        match outcome {
            Outcome::Up if ratio > max_ratio => {
                return SizingResult::reject(format!(
                    "UP inventory ratio too high: {:.2}",
                    ratio
                ));
            }
            Outcome::Down if (dec!(1) - ratio) > max_ratio => {
                return SizingResult::reject(format!(
                    "DOWN inventory ratio too high: {:.2}",
                    dec!(1) - ratio
                ));
            }
            _ => {}
        }

        // Minimum order size
        if size < dec!(5) {
            return SizingResult::allow(dec!(5));
        }

        // Round to whole shares
        let rounded = size.round();
        SizingResult::allow(rounded)
    }

    /// Inventory skew adjustments to widen/tighten quotes (global inventory).
    /// Returns (up_adjustment, down_adjustment) in price units.
    /// Note: the per-worker strategy uses `local_inventory_skew()` instead.
    pub fn inventory_skew_adjustment(&self) -> (Decimal, Decimal) {
        let ratio = self.state.inventory_ratio();
        let deviation = ratio - dec!(0.5);
        // Use max_inventory_ratio to derive threshold (midpoint between 50% and max)
        let threshold =
            Decimal::try_from(self.config.max_inventory_ratio - 0.5).unwrap_or(dec!(0.25));

        // Within half the threshold zone: no skew
        if deviation.abs() <= threshold / dec!(2) {
            return (dec!(0), dec!(0));
        }

        // Scale skew by deviation (larger imbalance → larger skew)
        let skew = deviation * dec!(0.02);
        (skew, -skew)
    }

    /// Check if we should cancel all orders before settlement.
    pub fn should_emergency_cancel(&self, seconds_to_settlement: i64) -> bool {
        seconds_to_settlement <= self.config.pre_settlement_cancel_secs
    }

    /// Record a trade result, update PnL and win/loss streak.
    pub fn record_trade_result(&self, pnl: Decimal) {
        self.state.add_daily_pnl(pnl);
        if pnl >= dec!(0) {
            self.state.record_win();
            info!("Trade result: +${:.4}", pnl);
        } else {
            let losses = self.state.record_loss();
            warn!("Trade loss: -${:.4} (consecutive: {})", pnl.abs(), losses);
        }
    }

    /// Reset daily counters (call at UTC midnight).
    pub fn reset_daily(&self) {
        self.state.reset_daily();
        info!("Daily risk counters reset");
    }

    pub fn metrics(&self) -> RiskMetrics {
        RiskMetrics {
            daily_pnl: self.state.get_daily_pnl(),
            inventory_up: self.state.get_inventory(Outcome::Up),
            inventory_down: self.state.get_inventory(Outcome::Down),
            inventory_ratio: self.state.inventory_ratio(),
            consecutive_losses: self.state.consecutive_losses(),
            is_paused: self.state.is_paused(),
            active_orders: self.state.active_orders.len(),
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RiskConfig;

    fn make_engine() -> RiskEngine {
        let state = BotState::new();
        let config = RiskConfig {
            bankroll: 1000.0,
            max_exposure_pct: 0.10,
            max_concurrent_markets: 1,
            daily_loss_limit_pct: 0.05,
            max_inventory_ratio: 0.75,
            circuit_breaker_losses: 3,
            pre_settlement_cancel_secs: 10,
        };
        RiskEngine::new(config, state)
    }

    #[test]
    fn test_can_trade_initial() {
        let engine = make_engine();
        let result = engine.can_trade();
        assert!(result.allowed);
    }

    #[test]
    fn test_circuit_breaker() {
        let engine = make_engine();
        for _ in 0..3 {
            engine.state.record_loss();
        }
        let result = engine.can_trade();
        assert!(!result.allowed);
        assert!(result.reason.contains("Circuit breaker"));
    }

    #[test]
    fn test_daily_loss_limit() {
        let engine = make_engine();
        // Add -$60 loss (6% of $1000 bankroll, limit is 5%)
        engine.state.add_daily_pnl(dec!(-60));
        let result = engine.can_trade();
        assert!(!result.allowed);
        assert!(result.reason.contains("Daily loss"));
    }

    #[test]
    fn test_half_kelly_sizing() {
        let engine = make_engine();
        // edge=0.05, odds=1.0 → kelly=0.05/1.0/2 = 0.025 → size=25
        let result = engine.calculate_size(dec!(0.05), dec!(1.0), Outcome::Up);
        assert!(result.allowed);
        assert_eq!(result.size, dec!(25));
    }

    #[test]
    fn test_minimum_size() {
        let engine = make_engine();
        // Very small edge → minimum 5 shares
        let result = engine.calculate_size(dec!(0.001), dec!(1.0), Outcome::Up);
        assert!(result.allowed);
        assert_eq!(result.size, dec!(5));
    }

    #[test]
    fn test_inventory_skew_balanced() {
        let engine = make_engine();
        // No inventory → balanced → no skew
        let (up, down) = engine.inventory_skew_adjustment();
        assert_eq!(up, dec!(0));
        assert_eq!(down, dec!(0));
    }

    #[test]
    fn test_emergency_cancel() {
        let engine = make_engine();
        assert!(engine.should_emergency_cancel(9));
        assert!(engine.should_emergency_cancel(10));
        assert!(!engine.should_emergency_cancel(11));
    }
}
