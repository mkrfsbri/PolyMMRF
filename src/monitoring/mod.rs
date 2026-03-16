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

        let market_slug = state
            .current_market
            .read()
            .as_ref()
            .map(|m| m.slug.clone())
            .unwrap_or_else(|| "none".to_string());

        info!(
            "[STATUS] market={} btc=${:.0} delta={:.2}% pnl=${:.2} \
             inv_up={:.1} inv_down={:.1} ratio={:.2} orders={} \
             losses={} paused={}",
            market_slug,
            btc_price,
            delta_pct,
            metrics.daily_pnl,
            metrics.inventory_up,
            metrics.inventory_down,
            metrics.inventory_ratio,
            metrics.active_orders,
            metrics.consecutive_losses,
            metrics.is_paused
        );
    }
}
