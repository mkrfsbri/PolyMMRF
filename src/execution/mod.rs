pub mod signing;

use crate::config::BotConfig;
use crate::execution::signing::{build_l2_headers, ClobCredentials};
use crate::types::{ActiveOrder, OrderRequest, OrderResponse, Outcome, Side};
use anyhow::{bail, Result};
use chrono::Utc;
use reqwest::Client;
use rust_decimal::Decimal;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};
use uuid::Uuid;

pub struct ExecutionEngine {
    config: BotConfig,
    client: Client,
    credentials: ClobCredentials,
    simulation: bool,
    state: Arc<crate::types::BotState>,
}

impl ExecutionEngine {
    pub fn new(config: BotConfig, state: Arc<crate::types::BotState>) -> Result<Self> {
        let simulation = config.bot.simulation;
        let credentials = ClobCredentials::from_env()?;
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;

        Ok(Self {
            config,
            client,
            credentials,
            simulation,
            state,
        })
    }

    fn auth_headers(
        &self,
        method: &str,
        path: &str,
        body: &str,
    ) -> Result<Vec<(String, String)>> {
        let mut headers = build_l2_headers(&self.credentials, method, path, body)?;
        headers.push(("POLY-ADDRESS".into(), self.credentials.address.clone()));
        Ok(headers)
    }

    pub async fn place_order(&self, req: OrderRequest) -> Result<OrderResponse> {
        if self.simulation {
            return self.simulate_order(req).await;
        }

        let nonce = Uuid::new_v4().to_string();
        let body = json!({
            "tokenID": req.token_id,
            "price": format!("{:.4}", req.price),
            "size": format!("{:.2}", req.size),
            "side": match req.side {
                Side::Buy => "BUY",
                Side::Sell => "SELL",
            },
            "feeRateBps": req.fee_rate_bps.to_string(),
            "nonce": nonce,
            "expiration": "0",
            "taker": "0x0000000000000000000000000000000000000000",
            "postOnly": req.post_only,
        });

        let body_str = body.to_string();
        let headers = self.auth_headers("POST", "/order", &body_str)?;
        let path = format!("{}/order", self.config.polymarket.clob_api_url);

        let mut request = self.client.post(&path).json(&body);
        for (k, v) in &headers {
            request = request.header(k.as_str(), v.as_str());
        }

        let resp = request.send().await?.error_for_status()?;
        let v: serde_json::Value = resp.json().await?;

        let order_id = v["orderID"]
            .as_str()
            .or_else(|| v["order_id"].as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing orderID in response"))?
            .to_string();

        let response = OrderResponse {
            order_id: order_id.clone(),
            status: v["status"]
                .as_str()
                .unwrap_or("live")
                .to_string(),
            price: req.price,
            size: req.size,
            side: req.side,
            token_id: req.token_id.clone(),
        };

        // Track in active orders
        self.state.active_orders.insert(
            order_id.clone(),
            ActiveOrder {
                order_id,
                token_id: req.token_id,
                outcome: req.outcome,
                side: req.side,
                price: req.price,
                size: req.size,
                filled: Decimal::ZERO,
                created_at: Utc::now(),
            },
        );

        info!(
            "Placed order: {} {} {} @ {}",
            response.order_id,
            match response.side { Side::Buy => "BUY", Side::Sell => "SELL" },
            response.size,
            response.price
        );

        Ok(response)
    }

    pub async fn cancel_order(&self, order_id: &str) -> Result<()> {
        if self.simulation {
            return self.simulate_cancel(order_id);
        }

        let body = json!({ "orderID": order_id });
        let body_str = body.to_string();
        let headers = self.auth_headers("DELETE", "/order", &body_str)?;
        let path = format!("{}/order", self.config.polymarket.clob_api_url);

        let mut request = self.client.delete(&path).json(&body);
        for (k, v) in &headers {
            request = request.header(k.as_str(), v.as_str());
        }

        match request.send().await {
            Ok(resp) => {
                if resp.status().is_success() {
                    debug!("Cancelled order: {}", order_id);
                } else {
                    warn!("Cancel returned non-success for {}: {}", order_id, resp.status());
                }
            }
            Err(e) => {
                warn!("Cancel request failed for {}: {}", order_id, e);
            }
        }

        self.state.active_orders.remove(order_id);
        Ok(())
    }

    pub async fn cancel_all_orders(&self) -> usize {
        let ids: Vec<String> = self
            .state
            .active_orders
            .iter()
            .map(|e| e.key().clone())
            .collect();
        let count = ids.len();
        for id in ids {
            let _ = self.cancel_order(&id).await;
        }
        info!("Cancelled {} orders", count);
        count
    }

    pub async fn cancel_and_replace(
        &self,
        old_id: &str,
        new_req: OrderRequest,
    ) -> Result<OrderResponse> {
        let _ = self.cancel_order(old_id).await;
        self.place_order(new_req).await
    }

    pub fn orders_for_outcome(&self, outcome: Outcome) -> Vec<ActiveOrder> {
        self.state
            .active_orders
            .iter()
            .filter(|e| e.value().outcome == outcome)
            .map(|e| e.value().clone())
            .collect()
    }

    // ── Simulation ────────────────────────────────────────────────────────────

    async fn simulate_order(&self, req: OrderRequest) -> Result<OrderResponse> {
        let order_id = format!("sim-{}", Uuid::new_v4());
        info!(
            "[SIM] Order: {} {} {} @ {}",
            order_id,
            match req.side { Side::Buy => "BUY", Side::Sell => "SELL" },
            req.size,
            req.price
        );

        let response = OrderResponse {
            order_id: order_id.clone(),
            status: "live".into(),
            price: req.price,
            size: req.size,
            side: req.side,
            token_id: req.token_id.clone(),
        };

        self.state.active_orders.insert(
            order_id.clone(),
            ActiveOrder {
                order_id,
                token_id: req.token_id,
                outcome: req.outcome,
                side: req.side,
                price: req.price,
                size: req.size,
                filled: Decimal::ZERO,
                created_at: Utc::now(),
            },
        );

        Ok(response)
    }

    fn simulate_cancel(&self, order_id: &str) -> Result<()> {
        debug!("[SIM] Cancel: {}", order_id);
        self.state.active_orders.remove(order_id);
        Ok(())
    }
}
