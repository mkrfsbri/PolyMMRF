use crate::types::{BotState, Outcome, Side};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::sync::Arc;
use tracing::info;

#[derive(Debug, Clone)]
pub struct SimFill {
    pub order_id: String,
    pub token_id: String,
    pub outcome: Outcome,
    pub side: Side,
    pub price: Decimal,
    pub size: Decimal,
}

pub struct SimFillEngine {
    state: Arc<BotState>,
    pub total_pnl: Decimal,
    pub fill_count: u32,
}

impl SimFillEngine {
    pub fn new(state: Arc<BotState>) -> Self {
        Self {
            state,
            total_pnl: dec!(0),
            fill_count: 0,
        }
    }

    /// Check simulated orders against live order book prices and fill if conditions met.
    pub fn check_fills(&mut self) -> Vec<SimFill> {
        let mut fills = Vec::new();
        let mut to_remove = Vec::new();

        // Collect order ids to process (only sim- prefix)
        let order_ids: Vec<String> = self
            .state
            .active_orders
            .iter()
            .filter(|e| e.key().starts_with("sim-"))
            .map(|e| e.key().clone())
            .collect();

        for order_id in order_ids {
            let Some(order) = self.state.active_orders.get(&order_id).map(|e| e.clone()) else {
                continue;
            };

            // Get orderbook for this token
            let Some(book) = self
                .state
                .order_books
                .get(&order.token_id)
                .map(|e| e.clone())
            else {
                continue;
            };

            let filled = match order.side {
                Side::Buy => {
                    // Buy fills if best ask <= order price
                    if let Some(best_ask) = book.best_ask() {
                        best_ask <= order.price
                    } else {
                        false
                    }
                }
                Side::Sell => {
                    // Sell fills if best bid >= order price
                    if let Some(best_bid) = book.best_bid() {
                        best_bid >= order.price
                    } else {
                        false
                    }
                }
            };

            if filled {
                info!(
                    "[SIM-FILL] {} {} {} @ {}",
                    order_id,
                    match order.side { Side::Buy => "BUY", Side::Sell => "SELL" },
                    order.size,
                    order.price
                );

                // Update inventory
                match order.side {
                    Side::Buy => {
                        self.state.add_inventory(order.outcome, order.size);
                    }
                    Side::Sell => {
                        self.state
                            .add_inventory(order.outcome, -order.size);
                    }
                }

                fills.push(SimFill {
                    order_id: order_id.clone(),
                    token_id: order.token_id.clone(),
                    outcome: order.outcome,
                    side: order.side,
                    price: order.price,
                    size: order.size,
                });

                to_remove.push(order_id);
                self.fill_count += 1;
            }
        }

        for id in to_remove {
            self.state.active_orders.remove(&id);
        }

        fills
    }

    /// Record a PnL amount computed externally (e.g. by the strategy from local inventory).
    /// Updates the running total used in `summary()`.
    pub fn record_pnl(&mut self, pnl: Decimal) {
        self.total_pnl += pnl;
    }

    pub fn summary(&self) -> String {
        let inv_up = self.state.get_inventory(Outcome::Up);
        let inv_down = self.state.get_inventory(Outcome::Down);
        format!(
            "Fills: {} | Total PnL: ${:.4} | Inventory UP={:.1} DOWN={:.1}",
            self.fill_count, self.total_pnl, inv_up, inv_down
        )
    }
}
