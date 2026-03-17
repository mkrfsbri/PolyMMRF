use crate::config::BotConfig;
use crate::execution::ExecutionEngine;
use crate::market_discovery::{time_remaining, MarketDiscovery};
use crate::risk::RiskEngine;
use crate::strategy::sim_fills::SimFillEngine;
use crate::types::{BotState, DataEvent, Market, MarketType, OrderRequest, Outcome, Side};
use anyhow::Result;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

// ── Quote Pair ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct QuotePair {
    pub up_bid: Decimal,
    pub up_ask: Decimal,
    pub down_bid: Decimal,
    pub down_ask: Decimal,
    pub size: Decimal,
}

// ── Strategy ──────────────────────────────────────────────────────────────────

pub struct MarketMakingStrategy {
    config: BotConfig,
    state: Arc<BotState>,
    execution: Arc<ExecutionEngine>,
    risk: Arc<RiskEngine>,
    discovery: Arc<MarketDiscovery>,
    sim_fills: Option<SimFillEngine>,
    up_order_ids: Vec<String>,
    down_order_ids: Vec<String>,
    last_quotes: Option<QuotePair>,
}

impl MarketMakingStrategy {
    pub fn new(
        config: BotConfig,
        state: Arc<BotState>,
        execution: Arc<ExecutionEngine>,
        risk: Arc<RiskEngine>,
        discovery: Arc<MarketDiscovery>,
    ) -> Self {
        let sim_fills = if config.bot.simulation {
            Some(SimFillEngine::new(state.clone()))
        } else {
            None
        };

        Self {
            config,
            state,
            execution,
            risk,
            discovery,
            sim_fills,
            up_order_ids: Vec::new(),
            down_order_ids: Vec::new(),
            last_quotes: None,
        }
    }

    pub async fn run(
        &mut self,
        mut event_rx: broadcast::Receiver<DataEvent>,
    ) -> Result<()> {
        loop {
            // 1. Find active market with >30s remaining
            let market = self.wait_for_market().await;

            info!(
                "Trading market: {} | ends in {}s",
                market.slug,
                market.seconds_remaining()
            );

            // Store in state
            *self.state.current_market.write() = Some(market.clone());

            // 2. Wait for market to stabilize
            tokio::time::sleep(Duration::from_secs(2)).await;

            // 3. Record opening BTC price
            let open_price = self.state.get_btc_price();
            self.state.set_window_open_price(open_price);
            info!("Window open price: ${}", open_price);

            // 4. Place initial quotes
            if let Err(e) = self.place_initial_quotes(&market).await {
                warn!("Failed to place initial quotes: {}", e);
            }

            // 5. Inner event loop
            let refresh_interval = Duration::from_millis(self.config.strategy.quote_refresh_ms);
            let mut refresh_timer = tokio::time::interval(refresh_interval);

            let mut market_done = false;
            while !market_done {
                tokio::select! {
                    _ = refresh_timer.tick() => {
                        let secs_left = market.seconds_remaining();

                        // Pre-settlement cancel
                        if self.risk.should_emergency_cancel(secs_left) {
                            info!("Pre-settlement cancel: {}s remaining", secs_left);
                            self.cancel_all_quotes().await;
                            market_done = true;
                            break;
                        }

                        // Refresh quotes
                        if let Err(e) = self.refresh_quotes(&market).await {
                            warn!("Quote refresh error: {}", e);
                        }

                        // Run sim fills
                        if let Some(ref mut sim) = self.sim_fills {
                            let fills = sim.check_fills();
                            if !fills.is_empty() {
                                info!("[SIM] {} fills this cycle", fills.len());
                            }
                        }

                        // Log metrics
                        let metrics = self.risk.metrics();
                        info!("Metrics: {}", metrics);
                    }

                    event = event_rx.recv() => {
                        match event {
                            Ok(DataEvent::OrderBookUpdate { token_id, book }) => {
                                self.state.order_books.insert(token_id, book);
                            }
                            Ok(DataEvent::PriceUpdate(price)) => {
                                debug!("BTC price update: {}", price.price);
                            }
                            Ok(DataEvent::MarketResolved { condition_id, winning_outcome }) => {
                                if condition_id == market.condition_id {
                                    info!("Market resolved: {:?} wins", winning_outcome);
                                    self.handle_settlement(&market, winning_outcome).await;
                                    market_done = true;
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(n)) => {
                                warn!("Event channel lagged by {} messages", n);
                            }
                            Err(_) => {
                                warn!("Event channel closed");
                                market_done = true;
                            }
                        }
                    }
                }
            }

            // 6. Cleanup
            self.cancel_all_quotes().await;
            self.state.reset_inventory();
            self.up_order_ids.clear();
            self.down_order_ids.clear();
            *self.state.current_market.write() = None;
            self.last_quotes = None;

            if let Some(ref sim) = self.sim_fills {
                info!("[SIM] Summary: {}", sim.summary());
            }

            info!("Market cycle complete, waiting 2s...");
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    async fn wait_for_market(&self) -> Market {
        let market_type = match self.config.strategy.market_type.as_str() {
            "15m" => MarketType::FifteenMinute,
            "5m" => MarketType::FiveMinute,
            _ => MarketType::Generic, // "generic" or any unrecognised value → keyword search
        };
        let asset = self
            .config
            .strategy
            .assets
            .first()
            .cloned()
            .unwrap_or_else(|| "BTC".into());

        loop {
            match self
                .discovery
                .find_active_market(&asset, market_type)
                .await
            {
                Ok(m) => {
                    let remaining = m.seconds_remaining();
                    if remaining > 30 {
                        return m;
                    } else {
                        info!(
                            "Market {} has only {}s left, waiting for next window...",
                            m.slug, remaining
                        );
                    }
                }
                Err(e) => {
                    warn!("Market discovery failed: {}", e);
                }
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    // ── Quote Calculation ─────────────────────────────────────────────────────

    fn calculate_quotes(&self, market: &Market) -> QuotePair {
        let base_price =
            Decimal::try_from(self.config.strategy.target_bid_price).unwrap_or(dec!(0.45));
        let half_spread =
            Decimal::try_from(self.config.strategy.half_spread).unwrap_or(dec!(0.03));
        let min_half =
            Decimal::try_from(self.config.strategy.min_spread / 2.0).unwrap_or(dec!(0.005));
        let max_half =
            Decimal::try_from(self.config.strategy.max_spread / 2.0).unwrap_or(dec!(0.05));

        // Time-decay adaptive spread — use actual start→end duration so
        // Generic markets (non-5m/15m) get correct spread decay (Bug #5 fix).
        let total_duration = market.actual_duration_secs();
        let remaining = market.seconds_remaining();
        let time_factor = if total_duration > 0 {
            Decimal::new(remaining, 0) / Decimal::new(total_duration, 0)
        } else {
            dec!(0.5)
        };
        // Wider early (time_factor=1), tighter near close (time_factor=0)
        let adaptive = half_spread * (dec!(0.5) + time_factor * dec!(0.5));
        let clamped_half = adaptive.max(min_half).min(max_half);

        // Inventory skew
        let (up_skew, down_skew) = self.risk.inventory_skew_adjustment();

        // UP quotes
        let up_bid = (base_price - clamped_half + up_skew)
            .max(dec!(0.01))
            .min(dec!(0.99));
        let up_ask = (base_price + clamped_half + up_skew)
            .max(dec!(0.01))
            .min(dec!(0.99));

        // DOWN quotes (symmetric)
        let down_bid = (base_price - clamped_half + down_skew)
            .max(dec!(0.01))
            .min(dec!(0.99));
        let down_ask = (base_price + clamped_half + down_skew)
            .max(dec!(0.01))
            .min(dec!(0.99));

        // Size from risk engine or fallback to config
        let order_size =
            Decimal::try_from(self.config.strategy.order_size).unwrap_or(dec!(10));
        let sizing = self.risk.calculate_size(dec!(0.05), dec!(1.0), Outcome::Up);
        let size = if sizing.allowed {
            sizing.size.min(order_size)
        } else {
            order_size
        };

        // Round to tick (0.01)
        let tick = dec!(0.01);
        let round_to_tick = |p: Decimal| -> Decimal { (p / tick).round() * tick };

        QuotePair {
            up_bid: round_to_tick(up_bid),
            up_ask: round_to_tick(up_ask),
            down_bid: round_to_tick(down_bid),
            down_ask: round_to_tick(down_ask),
            size,
        }
    }

    fn should_update_quotes(&self, new_quotes: &QuotePair) -> bool {
        let min_change = dec!(0.01);
        if let Some(ref last) = self.last_quotes {
            (new_quotes.up_bid - last.up_bid).abs() >= min_change
                || (new_quotes.down_bid - last.down_bid).abs() >= min_change
        } else {
            true
        }
    }

    // ── Quote Placement ────────────────────────────────────────────────────────

    async fn place_initial_quotes(&mut self, market: &Market) -> Result<()> {
        let gate = self.risk.can_trade();
        if !gate.allowed {
            warn!("Risk gate blocked: {}", gate.reason);
            return Ok(());
        }

        let quotes = self.calculate_quotes(market);
        info!(
            "Initial quotes: UP bid={} DOWN bid={} size={}",
            quotes.up_bid, quotes.down_bid, quotes.size
        );

        // Place UP BUY limit
        let up_resp = self
            .execution
            .place_order(OrderRequest {
                token_id: market.token_id_up.clone(),
                side: Side::Buy,
                price: quotes.up_bid,
                size: quotes.size,
                outcome: Outcome::Up,
                fee_rate_bps: market.fee_rate_bps,
                post_only: self.config.strategy.post_only,
            })
            .await?;
        self.up_order_ids.push(up_resp.order_id);

        // Place DOWN BUY limit
        let down_resp = self
            .execution
            .place_order(OrderRequest {
                token_id: market.token_id_down.clone(),
                side: Side::Buy,
                price: quotes.down_bid,
                size: quotes.size,
                outcome: Outcome::Down,
                fee_rate_bps: market.fee_rate_bps,
                post_only: self.config.strategy.post_only,
            })
            .await?;
        self.down_order_ids.push(down_resp.order_id);

        self.last_quotes = Some(quotes);
        Ok(())
    }

    async fn refresh_quotes(&mut self, market: &Market) -> Result<()> {
        let new_quotes = self.calculate_quotes(market);
        if !self.should_update_quotes(&new_quotes) {
            debug!("Quotes unchanged, skipping refresh");
            return Ok(());
        }

        // Cancel existing then place new
        self.cancel_all_quotes().await;
        self.place_initial_quotes(market).await
    }

    async fn cancel_all_quotes(&mut self) {
        let up_ids: Vec<String> = self.up_order_ids.drain(..).collect();
        let down_ids: Vec<String> = self.down_order_ids.drain(..).collect();

        for id in up_ids.iter().chain(down_ids.iter()) {
            let _ = self.execution.cancel_order(id).await;
        }
    }

    // ── Settlement ────────────────────────────────────────────────────────────

    async fn handle_settlement(&mut self, market: &Market, winning_outcome: Outcome) {
        info!("Handling settlement: {:?} wins", winning_outcome);

        let inv_up = self.state.get_inventory(Outcome::Up);
        let inv_down = self.state.get_inventory(Outcome::Down);

        if let Some(ref mut sim) = self.sim_fills {
            sim.simulate_settlement(winning_outcome);
        } else {
            // Live settlement PnL estimation
            let cost = dec!(0.45);
            let pnl = match winning_outcome {
                Outcome::Up => {
                    inv_up * (dec!(1) - cost) - inv_down * cost
                }
                Outcome::Down => {
                    inv_down * (dec!(1) - cost) - inv_up * cost
                }
            };
            self.risk.record_trade_result(pnl);
        }

        self.cancel_all_quotes().await;
    }

    pub fn time_to_settlement(market: &Market) -> i64 {
        market.seconds_remaining()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BotConfig, RiskConfig};
    use chrono::{Duration as CDuration, Utc};
    use crate::types::MarketType;

    fn make_strategy() -> MarketMakingStrategy {
        let config = BotConfig::default();
        let state = BotState::new();
        let risk = Arc::new(RiskEngine::new(config.risk.clone(), state.clone()));
        let discovery = Arc::new(MarketDiscovery::new(config.clone()).unwrap());
        let execution = Arc::new(ExecutionEngine::new(config.clone(), state.clone()).unwrap());
        MarketMakingStrategy::new(config, state, execution, risk, discovery)
    }

    fn make_market() -> Market {
        Market {
            condition_id: "0x1234".into(),
            slug: "btc-updown-5m-1710000000".into(),
            question: "Will BTC go up?".into(),
            token_id_up: "up-token".into(),
            token_id_down: "down-token".into(),
            start_time: Utc::now(),
            end_time: Utc::now() + CDuration::seconds(300),
            market_type: MarketType::FiveMinute,
            asset: "BTC".into(),
            fee_rate_bps: 315,
            neg_risk: false,
        }
    }

    #[test]
    fn test_calculate_quotes_balanced() {
        let strategy = make_strategy();
        let market = make_market();
        let quotes = strategy.calculate_quotes(&market);

        // With target=0.45, half_spread=0.03 (default)
        // UP bid should be around 0.42..0.45 range
        assert!(quotes.up_bid > dec!(0.01));
        assert!(quotes.up_bid < dec!(0.99));
        assert!(quotes.up_ask > quotes.up_bid);
        assert!(quotes.down_bid > dec!(0.01));
    }

    #[test]
    fn test_tick_rounding() {
        let strategy = make_strategy();
        let market = make_market();
        let quotes = strategy.calculate_quotes(&market);

        // All prices should be multiples of 0.01
        let tick = dec!(0.01);
        assert_eq!(quotes.up_bid % tick, dec!(0));
        assert_eq!(quotes.down_bid % tick, dec!(0));
    }

    #[test]
    fn test_should_update_quotes_initial() {
        let strategy = make_strategy();
        let market = make_market();
        let quotes = strategy.calculate_quotes(&market);
        // No previous quotes → should update
        assert!(strategy.should_update_quotes(&quotes));
    }
}
