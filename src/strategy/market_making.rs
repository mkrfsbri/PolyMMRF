use crate::config::BotConfig;
use crate::execution::ExecutionEngine;
use crate::market_discovery::MarketDiscovery;
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
    /// "5m", "15m", or "generic" — determines which market type this worker targets.
    market_type_str: String,
    config: BotConfig,
    state: Arc<BotState>,
    execution: Arc<ExecutionEngine>,
    risk: Arc<RiskEngine>,
    sim_fills: Option<SimFillEngine>,
    up_order_ids: Vec<String>,
    down_order_ids: Vec<String>,
    last_quotes: Option<QuotePair>,
    /// Per-worker inventory — isolated from other market workers.
    /// Resets at the end of each market cycle.
    local_inv_up: Decimal,
    local_inv_down: Decimal,
}

impl MarketMakingStrategy {
    pub fn new(
        market_type_str: String,
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

        // discovery is held by the caller (MarketDiscovery is passed to wait_for_market
        // indirectly — store it here)
        let _ = discovery; // used via find_active_market calls in wait_for_market

        Self {
            market_type_str,
            config,
            state,
            execution,
            risk,
            sim_fills,
            up_order_ids: Vec::new(),
            down_order_ids: Vec::new(),
            last_quotes: None,
            local_inv_up: Decimal::ZERO,
            local_inv_down: Decimal::ZERO,
        }
    }

    pub async fn run(
        &mut self,
        mut event_rx: broadcast::Receiver<DataEvent>,
        discovery: Arc<MarketDiscovery>,
    ) -> Result<()> {
        loop {
            // 1. Find active market with sufficient time remaining
            let market = self.wait_for_market(&discovery).await;

            info!(
                "[{}] Trading market: {} | ends in {}s",
                self.market_type_str,
                market.slug,
                market.seconds_remaining()
            );

            // Store in per-worker slot of current_markets
            self.state
                .current_markets
                .insert(self.market_type_str.clone(), market.clone());

            // 2. Wait for market to stabilize
            tokio::time::sleep(Duration::from_secs(2)).await;

            // 3. Record opening BTC price
            let open_price = self.state.get_btc_price();
            self.state.set_window_open_price(open_price);
            info!("[{}] Window open price: ${}", self.market_type_str, open_price);

            // 4. Place initial quotes
            if let Err(e) = self.place_initial_quotes(&market).await {
                warn!("[{}] Failed to place initial quotes: {}", self.market_type_str, e);
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
                            info!(
                                "[{}] Pre-settlement cancel: {}s remaining",
                                self.market_type_str, secs_left
                            );
                            self.cancel_all_quotes().await;
                            market_done = true;
                            break;
                        }

                        // Refresh quotes
                        if let Err(e) = self.refresh_quotes(&market).await {
                            warn!("[{}] Quote refresh error: {}", self.market_type_str, e);
                        }

                        // Run sim fills and accumulate into local inventory
                        if let Some(ref mut sim) = self.sim_fills {
                            let fills = sim.check_fills();
                            for fill in &fills {
                                match fill.side {
                                    Side::Buy => match fill.outcome {
                                        Outcome::Up => self.local_inv_up += fill.size,
                                        Outcome::Down => self.local_inv_down += fill.size,
                                    },
                                    Side::Sell => match fill.outcome {
                                        Outcome::Up => self.local_inv_up -= fill.size,
                                        Outcome::Down => self.local_inv_down -= fill.size,
                                    },
                                }
                            }
                            if !fills.is_empty() {
                                info!(
                                    "[SIM][{}] {} fills | inv UP={:.1} DOWN={:.1}",
                                    self.market_type_str,
                                    fills.len(),
                                    self.local_inv_up,
                                    self.local_inv_down,
                                );
                            }
                        }

                        // Log metrics
                        let metrics = self.risk.metrics();
                        info!("[{}] {}", self.market_type_str, metrics);
                    }

                    event = event_rx.recv() => {
                        match event {
                            Ok(DataEvent::OrderBookUpdate { token_id, book }) => {
                                self.state.order_books.insert(token_id, book);
                            }
                            Ok(DataEvent::PriceUpdate(price)) => {
                                debug!("[{}] BTC price update: {}", self.market_type_str, price.price);
                            }
                            Ok(DataEvent::MarketResolved { condition_id, winning_outcome }) => {
                                if condition_id == market.condition_id {
                                    info!(
                                        "[{}] Market resolved: {:?} wins",
                                        self.market_type_str, winning_outcome
                                    );
                                    self.handle_settlement(&market, winning_outcome).await;
                                    market_done = true;
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(n)) => {
                                warn!("[{}] Event channel lagged by {} messages", self.market_type_str, n);
                            }
                            Err(_) => {
                                warn!("[{}] Event channel closed", self.market_type_str);
                                market_done = true;
                            }
                        }
                    }
                }
            }

            // 6. Cleanup
            self.cancel_all_quotes().await;
            // Reset per-worker local inventory (not shared state)
            self.local_inv_up = Decimal::ZERO;
            self.local_inv_down = Decimal::ZERO;
            self.up_order_ids.clear();
            self.down_order_ids.clear();
            self.state.current_markets.remove(&self.market_type_str);
            self.last_quotes = None;

            if let Some(ref sim) = self.sim_fills {
                info!("[SIM][{}] Summary: {}", self.market_type_str, sim.summary());
            }

            info!(
                "[{}] Market cycle complete, waiting 2s...",
                self.market_type_str
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    async fn wait_for_market(&self, discovery: &MarketDiscovery) -> Market {
        let market_type = match self.market_type_str.as_str() {
            "15m" => MarketType::FifteenMinute,
            "5m" => MarketType::FiveMinute,
            _ => MarketType::Generic,
        };
        let asset = self
            .config
            .strategy
            .assets
            .first()
            .cloned()
            .unwrap_or_else(|| "BTC".into());

        loop {
            match discovery.find_active_market(&asset, market_type).await {
                Ok(m) => {
                    let remaining = m.seconds_remaining();
                    let min_secs = self.config.strategy.min_market_secs_remaining;
                    if remaining > min_secs {
                        // Guard: don't claim a market already being traded by another
                        // worker. This prevents both "5m" and "15m" workers from
                        // selecting the same Tier-3 generic market simultaneously.
                        let already_claimed = self.state.current_markets.iter().any(|e| {
                            e.key() != &self.market_type_str
                                && e.value().condition_id == m.condition_id
                        });
                        if already_claimed {
                            info!(
                                "[{}] Market '{}' already claimed by another worker — \
                                 waiting for a dedicated window market...",
                                self.market_type_str, m.slug
                            );
                            tokio::time::sleep(Duration::from_secs(30)).await;
                            continue;
                        }
                        return m;
                    } else {
                        info!(
                            "[{}] Market {} has only {}s left (need >{}s), waiting...",
                            self.market_type_str, m.slug, remaining, min_secs
                        );
                    }
                }
                Err(e) => {
                    warn!("[{}] Market discovery failed: {}", self.market_type_str, e);
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

        // Time-decay adaptive spread
        let total_duration = market.actual_duration_secs();
        let remaining = market.seconds_remaining();
        let time_factor = if total_duration > 0 {
            Decimal::new(remaining, 0) / Decimal::new(total_duration, 0)
        } else {
            dec!(0.5)
        };
        let adaptive = half_spread * (dec!(0.5) + time_factor * dec!(0.5));
        let clamped_half = adaptive.max(min_half).min(max_half);

        // Per-worker inventory skew (isolated from other market workers)
        let (up_skew, down_skew) = self.local_inventory_skew();

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

    /// Compute inventory skew from this worker's local inventory only.
    /// Prevents cross-market interference when trading 5m and 15m simultaneously.
    fn local_inventory_skew(&self) -> (Decimal, Decimal) {
        let total = self.local_inv_up + self.local_inv_down;
        if total.is_zero() {
            return (dec!(0), dec!(0));
        }
        let ratio = self.local_inv_up / total;
        let deviation = ratio - dec!(0.5);
        let threshold =
            Decimal::try_from(self.config.risk.max_inventory_ratio - 0.5).unwrap_or(dec!(0.25));
        if deviation.abs() <= threshold / dec!(2) {
            return (dec!(0), dec!(0));
        }
        // 2 cents skew per 10% deviation
        let skew = deviation * dec!(0.02);
        (skew, -skew)
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
            warn!("[{}] Risk gate blocked: {}", self.market_type_str, gate.reason);
            return Ok(());
        }

        let quotes = self.calculate_quotes(market);
        info!(
            "[{}] Initial quotes: UP bid={} DOWN bid={} size={}",
            self.market_type_str, quotes.up_bid, quotes.down_bid, quotes.size
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
            debug!("[{}] Quotes unchanged, skipping refresh", self.market_type_str);
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

    async fn handle_settlement(&mut self, _market: &Market, winning_outcome: Outcome) {
        info!(
            "[{}] Handling settlement: {:?} wins | inv UP={:.1} DOWN={:.1}",
            self.market_type_str, winning_outcome, self.local_inv_up, self.local_inv_down
        );

        if let Some(ref mut sim) = self.sim_fills {
            // In sim mode, calculate PnL from local (per-worker) inventory
            let cost_basis = dec!(0.45);
            let pnl = match winning_outcome {
                Outcome::Up => {
                    self.local_inv_up * (dec!(1) - cost_basis) - self.local_inv_down * cost_basis
                }
                Outcome::Down => {
                    self.local_inv_down * (dec!(1) - cost_basis) - self.local_inv_up * cost_basis
                }
            };
            sim.record_pnl(pnl);
            self.risk.record_trade_result(pnl);
            info!("[SIM][{}] Settlement PnL: ${:.4}", self.market_type_str, pnl);
        } else {
            // Live settlement PnL estimation from local inventory
            let cost = dec!(0.45);
            let pnl = match winning_outcome {
                Outcome::Up => {
                    self.local_inv_up * (dec!(1) - cost) - self.local_inv_down * cost
                }
                Outcome::Down => {
                    self.local_inv_down * (dec!(1) - cost) - self.local_inv_up * cost
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
    use crate::config::BotConfig;
    use crate::market_discovery::MarketDiscovery;
    use crate::execution::ExecutionEngine;
    use chrono::{Duration as CDuration, Utc};
    use crate::types::MarketType;

    fn make_strategy() -> (MarketMakingStrategy, Arc<MarketDiscovery>) {
        let config = BotConfig::default();
        let state = BotState::new();
        let risk = Arc::new(RiskEngine::new(config.risk.clone(), state.clone()));
        let discovery = Arc::new(MarketDiscovery::new(config.clone()).unwrap());
        let execution = Arc::new(ExecutionEngine::new(config.clone(), state.clone()).unwrap());
        let strategy = MarketMakingStrategy::new(
            "5m".into(),
            config,
            state,
            execution,
            risk,
            discovery.clone(),
        );
        (strategy, discovery)
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
        let (strategy, _) = make_strategy();
        let market = make_market();
        let quotes = strategy.calculate_quotes(&market);

        assert!(quotes.up_bid > dec!(0.01));
        assert!(quotes.up_bid < dec!(0.99));
        assert!(quotes.up_ask > quotes.up_bid);
        assert!(quotes.down_bid > dec!(0.01));
    }

    #[test]
    fn test_tick_rounding() {
        let (strategy, _) = make_strategy();
        let market = make_market();
        let quotes = strategy.calculate_quotes(&market);

        let tick = dec!(0.01);
        assert_eq!(quotes.up_bid % tick, dec!(0));
        assert_eq!(quotes.down_bid % tick, dec!(0));
    }

    #[test]
    fn test_should_update_quotes_initial() {
        let (strategy, _) = make_strategy();
        let market = make_market();
        let quotes = strategy.calculate_quotes(&market);
        assert!(strategy.should_update_quotes(&quotes));
    }

    #[test]
    fn test_local_inventory_skew_balanced() {
        let (strategy, _) = make_strategy();
        // No inventory → no skew
        let (up, down) = strategy.local_inventory_skew();
        assert_eq!(up, dec!(0));
        assert_eq!(down, dec!(0));
    }

    #[test]
    fn test_market_type_str_field() {
        let (strategy, _) = make_strategy();
        assert_eq!(strategy.market_type_str, "5m");
    }
}
