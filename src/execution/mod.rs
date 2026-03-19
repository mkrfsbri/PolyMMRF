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
        let mut credentials = ClobCredentials::from_env()?;

        // For EOA (sig_type 0) the POLY-ADDRESS header must be the signing EOA,
        // not POLY_FUNDER_ADDRESS (which may still hold an old proxy address).
        // Derive it from the private key and override credentials.address so that
        // ALL L2 auth calls (place_order, cancel_order, validation) use the right
        // address without needing per-call knowledge of sig_type.
        if config.polymarket.signature_type == 0 && !config.polymarket.private_key.is_empty() {
            use alloy::signers::local::PrivateKeySigner;
            if let Ok(signer) = config.polymarket.private_key.parse::<PrivateKeySigner>() {
                credentials.address = format!("{:?}", signer.address());
            }
        }

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
        self.auth_headers_with_address(method, path, body, &self.credentials.address)
    }

    fn auth_headers_with_address(
        &self,
        method: &str,
        path: &str,
        body: &str,
        address: &str,
    ) -> Result<Vec<(String, String)>> {
        let mut headers = build_l2_headers(&self.credentials, method, path, body)?;
        headers.push(("POLY-ADDRESS".into(), address.to_string()));
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

        let sig_type = self.config.polymarket.signature_type;

        // For PROXY / GnosisSafe (sig_type > 0) the funder address is the maker in
        // the EIP-712 struct and must be set.  For EOA (sig_type == 0) the maker is
        // derived from the private key so funder_address is not consulted.
        if sig_type > 0 && funder_address.is_empty() {
            bail!(
                "POLY_FUNDER_ADDRESS is not set — required for PROXY / GnosisSafe mode.\n  \
                 This is the address of your Polymarket proxy or Gnosis Safe wallet.\n  \
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
            sig_type,
            req.neg_risk,
        )
        .await
        .map_err(|e| anyhow::anyhow!("Order signing failed: {}", e))?;

        // For EOA (sig_type 0) the contract requires maker == signer (same address).
        // For POLY_PROXY (sig_type 1) maker = proxy wallet, signer = controlling EOA.
        let order_maker = if sig_type == 0 {
            signer_addr.as_str()
        } else {
            funder_address.as_str()
        };

        // Field types must match exactly what Polymarket's CLOB expects:
        //   uint256 fields → decimal strings (JS can't hold uint256 as a number)
        //   uint8 fields   → JSON integers (side, signatureType)
        //   address fields → lowercase hex strings
        let body = json!({
            "order": {
                "salt": salt,
                "maker": order_maker,
                "signer": signer_addr,
                "taker": "0x0000000000000000000000000000000000000000",
                "tokenId": req.token_id,
                "makerAmount": maker_amount.to_string(),
                "takerAmount": taker_amount.to_string(),
                "expiration": "0",
                "nonce": "0",
                "feeRateBps": req.fee_rate_bps.to_string(),
                "side": side_u8,
                "signatureType": sig_type,
                "signature": signature,
            },
            "owner": order_maker,
            "orderType": "GTC",
        });

        let body_str = body.to_string();
        let headers = self.auth_headers_with_address("POST", "/order", &body_str, order_maker)?;
        let path = format!("{}/order", self.config.polymarket.clob_api_url);

        let mut request = self.client.post(&path).json(&body);
        for (k, v) in &headers {
            request = request.header(k.as_str(), v.as_str());
        }

        let http_resp = request.send().await?;
        let status = http_resp.status();
        if !status.is_success() {
            let raw_body = http_resp.text().await.unwrap_or_default();
            match status {
                reqwest::StatusCode::UNAUTHORIZED => warn!(
                    "POST /order → 401 Unauthorized\n  \
                     Polymarket response: {}\n  \
                     maker={} signer={} sig_type={}\n  \
                     Check POLY_API_KEY/SECRET/PASSPHRASE match the account for this signer.",
                    raw_body, order_maker, signer_addr, sig_type
                ),
                reqwest::StatusCode::FORBIDDEN => warn!(
                    "POST /order → 403 Forbidden\n  \
                     Polymarket response: {}\n  \
                     Ensure POLY_API_KEY, POLY_API_SECRET, POLY_API_PASSPHRASE are set.",
                    raw_body
                ),
                _ => warn!("POST /order → HTTP {}: {}", status, raw_body),
            }
            bail!("HTTP status client error ({} {}) for url ({})", status.as_u16(), status.canonical_reason().unwrap_or(""), format!("{}/order", self.config.polymarket.clob_api_url));
        }
        let resp = http_resp;
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
