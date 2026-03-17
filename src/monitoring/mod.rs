pub mod trade_logger;

use crate::risk::RiskEngine;
use crate::types::BotState;
use std::sync::Arc;
use tokio::time::{interval, Duration};
use tracing::info;

/// Periodic status logger — logs key metrics every 30 seconds.
pub async fn monitoring_loop(state: Arc<BotState>, risk: Arc<RiskEngine>) {
    let mut ticker = interval(Duration::from_secs(30));

    loop {
        ticker.tick().await;

        let btc_price = state.get_btc_price();
        let open_price = state.get_window_open_price();
        let metrics = risk.metrics();

        let delta_pct = if open_price.is_zero() {
            rust_decimal_macros::dec!(0)
        } else {
            ((btc_price - open_price) / open_price) * rust_decimal_macros::dec!(100)
        };

        // Collect slugs from all active market workers ("5m", "15m", etc.)
        let mut market_slugs: Vec<String> = state
            .current_markets
            .iter()
            .map(|e| format!("{}:{}", e.key(), e.value().slug))
            .collect();
        market_slugs.sort();
        let market_str = if market_slugs.is_empty() {
            "none".to_string()
        } else {
            market_slugs.join(", ")
        };

        info!(
            "[STATUS] markets=[{}] btc=${:.0} delta={:.2}% pnl=${:.2} orders={} losses={} paused={}",
            market_str,
            btc_price,
            delta_pct,
            metrics.daily_pnl,
            metrics.active_orders,
            metrics.consecutive_losses,
            metrics.is_paused
        );
    }
}
