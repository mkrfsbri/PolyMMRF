pub mod signing;

use crate::config::BotConfig;
use crate::execution::signing::{
    build_l2_headers, calculate_amounts, sign_clob_order, ClobCredentials,
};
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

        // Warn early if running live with missing credentials
        if !simulation {
            let missing: Vec<&str> = [
                ("POLY_API_KEY", credentials.api_key.is_empty()),
                ("POLY_API_SECRET", credentials.api_secret.is_empty()),
                ("POLY_API_PASSPHRASE", credentials.api_passphrase.is_empty()),
                ("POLY_FUNDER_ADDRESS", credentials.address.is_empty()),
            ]
            .iter()
            .filter_map(|(name, empty)| if *empty { Some(*name) } else { None })
            .collect();

            if !missing.is_empty() {
                warn!(
                    "LIVE TRADING: missing API credentials: {:?}\n  \
                     Orders will fail with 403 Forbidden until these are set.\n  \
                     Set them via environment variables or in .env file.\n  \
                     Use `simulation = true` in config.toml to test without credentials.",
                    missing
                );
            }
        }

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

        let side_u8: u8 = match req.side {
            Side::Buy => 0,
            Side::Sell => 1,
        };

        let (maker_amount, taker_amount) =
            calculate_amounts(req.price, req.size, &req.side, req.neg_risk);

        // Build EIP-712 signed order.
        // POLY_PRIVATE_KEY  — Ethereum private key for the trading wallet
        // POLY_FUNDER_ADDRESS — address of the Gnosis Safe / proxy wallet (maker)
        let private_key = &self.config.polymarket.private_key;
        let funder_address = &self.config.polymarket.funder_address;

        if private_key.is_empty() {
            bail!(
                "POLY_PRIVATE_KEY is not set — required for live order placement.\n  \
                 Set it as an environment variable or in your .env file.\n  \
                 Obtain your key at https://polymarket.com/profile?tab=api-keys"
            );
        }
        if funder_address.is_empty() {
            bail!(
                "POLY_FUNDER_ADDRESS is not set — required for live order placement.\n  \
                 This is the address of your Polymarket wallet (Gnosis Safe or EOA).\n  \
                 Obtain it at https://polymarket.com/profile?tab=api-keys"
            );
        }

        let (signature, signer_addr, salt) = sign_clob_order(
            private_key,
            funder_address,
            &req.token_id,
            maker_amount,
            taker_amount,
            side_u8,
            req.fee_rate_bps,
            self.config.polymarket.signature_type,
            req.neg_risk,
        )
        .await
        .map_err(|e| anyhow::anyhow!("Order signing failed: {}", e))?;

        let body = json!({
            "order": {
                "salt": salt,
                "maker": funder_address,
                "signer": signer_addr,
                "taker": "0x0000000000000000000000000000000000000000",
                "tokenId": req.token_id,
                "makerAmount": maker_amount.to_string(),
                "takerAmount": taker_amount.to_string(),
                "expiration": "0",
                "nonce": "0",
                "feeRateBps": req.fee_rate_bps.to_string(),
                "side": side_u8.to_string(),
                "signatureType": self.config.polymarket.signature_type.to_string(),
                "signature": signature,
            },
            "owner": funder_address,
            "orderType": if req.post_only { "GTD" } else { "GTC" },
        });

        let body_str = body.to_string();
        let headers = self.auth_headers("POST", "/order", &body_str)?;
        let path = format!("{}/order", self.config.polymarket.clob_api_url);

        let mut request = self.client.post(&path).json(&body);
        for (k, v) in &headers {
            request = request.header(k.as_str(), v.as_str());
        }

        let http_resp = request.send().await?;
        let status = http_resp.status();
        if status == reqwest::StatusCode::FORBIDDEN {
            warn!(
                "Order placement returned 403 Forbidden — API credentials invalid or missing.\n  \
                 Required env vars: POLY_API_KEY, POLY_API_SECRET, POLY_API_PASSPHRASE, POLY_FUNDER_ADDRESS\n  \
                 Get credentials from: https://polymarket.com/profile?tab=api-keys"
            );
        } else if status == reqwest::StatusCode::UNAUTHORIZED {
            warn!(
                "Order placement returned 401 Unauthorized — HMAC signature or EIP-712 signature is invalid.\n  \
                 Check that POLY_API_SECRET is exactly as shown at https://polymarket.com/profile?tab=api-keys\n  \
                 and POLY_PRIVATE_KEY matches the wallet registered with Polymarket."
            );
        }
        let resp = http_resp.error_for_status()?;
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
