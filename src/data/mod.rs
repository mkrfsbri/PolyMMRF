use crate::config::CoinbaseConfig;
use crate::types::{BtcPrice, DataEvent, OrderBook, PriceLevel, PriceSource};
use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use rust_decimal::Decimal;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

// ── BTC Price Feed (Binance WS + Coinbase REST fallback) ──────────────────────

/// Top-level BTC price feed.  Tries Binance WS first; after
/// `coinbase_cfg.max_binance_failures` consecutive connection failures it
/// switches to polling the Coinbase Advanced Trade REST endpoint.  While in
/// fallback mode it retries Binance every `retry_binance_secs` seconds and
/// switches back transparently once it reconnects.
pub async fn run_btc_price_feed(
    binance_ws_url: String,
    coinbase_cfg: CoinbaseConfig,
    state: Arc<crate::types::BotState>,
    tx: broadcast::Sender<DataEvent>,
) {
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("HTTP client build failed");

    let mut consecutive_failures: u32 = 0;

    loop {
        if !coinbase_cfg.enabled || consecutive_failures < coinbase_cfg.max_binance_failures {
            // ── Try Binance WebSocket ────────────────────────────────────────
            info!("Connecting to Binance WebSocket: {}", binance_ws_url);
            match connect_async(&binance_ws_url).await {
                Ok((ws_stream, _)) => {
                    info!("Binance WebSocket connected (source: primary)");
                    consecutive_failures = 0;

                    let (mut write, mut read) = ws_stream.split();
                    let mut clean_exit = true;

                    while let Some(msg) = read.next().await {
                        match msg {
                            Ok(Message::Text(text)) => {
                                if let Some(price) = parse_binance_ticker(&text) {
                                    debug!("BTC/Binance: ${}", price.price);
                                    state.set_btc_price(price.price);
                                    let _ = tx.send(DataEvent::PriceUpdate(price));
                                }
                            }
                            Ok(Message::Ping(data)) => {
                                let _ = write.send(Message::Pong(data)).await;
                            }
                            Ok(Message::Close(_)) => {
                                warn!("Binance WS closed by server");
                                break;
                            }
                            Err(e) => {
                                error!("Binance WS error: {}", e);
                                clean_exit = false;
                                break;
                            }
                            _ => {}
                        }
                    }

                    if !clean_exit {
                        consecutive_failures += 1;
                        warn!(
                            "Binance WS failure #{} (max before fallback: {})",
                            consecutive_failures, coinbase_cfg.max_binance_failures
                        );
                    }
                }
                Err(e) => {
                    consecutive_failures += 1;
                    error!(
                        "Binance WS connect error (failure #{}/{}): {}",
                        consecutive_failures, coinbase_cfg.max_binance_failures, e
                    );
                }
            }

            let backoff = if consecutive_failures == 0 { 3 } else { 3 };
            tokio::time::sleep(Duration::from_secs(backoff)).await;
        } else {
            // ── Coinbase REST fallback ───────────────────────────────────────
            warn!(
                "Binance WS unavailable after {} failures — activating Coinbase REST fallback",
                consecutive_failures
            );

            run_coinbase_fallback(
                &coinbase_cfg,
                &client,
                &state,
                &tx,
            )
            .await;

            // After the fallback period expires, reset failure count and retry
            // Binance.  run_coinbase_fallback returns after retry_binance_secs.
            info!("Retrying Binance WebSocket after fallback period...");
            consecutive_failures = 0;
        }
    }
}

/// Poll Coinbase REST for `retry_binance_secs` then return so the caller can
/// attempt Binance again.  Each poll fires every `poll_interval_ms`.
async fn run_coinbase_fallback(
    cfg: &CoinbaseConfig,
    client: &Client,
    state: &Arc<crate::types::BotState>,
    tx: &broadcast::Sender<DataEvent>,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(cfg.retry_binance_secs);
    let mut interval = tokio::time::interval(Duration::from_millis(cfg.poll_interval_ms));

    loop {
        interval.tick().await;

        match fetch_coinbase_price(&cfg.rest_url, client).await {
            Ok(price) => {
                debug!("BTC/Coinbase: ${}", price.price);
                state.set_btc_price(price.price);
                let _ = tx.send(DataEvent::PriceUpdate(price));
            }
            Err(e) => {
                warn!("Coinbase REST fetch failed: {}", e);
            }
        }

        if tokio::time::Instant::now() >= deadline {
            break;
        }
    }
}

/// Fetch BTC/USD spot price from Coinbase Advanced Trade API (no auth needed).
///
/// Response shape:
/// ```json
/// {"data": {"base": "BTC", "currency": "USD", "amount": "65432.10"}}
/// ```
pub async fn fetch_coinbase_price(rest_url: &str, client: &Client) -> Result<BtcPrice> {
    let resp = client
        .get(rest_url)
        .header("Content-Type", "application/json")
        .send()
        .await?
        .error_for_status()?
        .json::<serde_json::Value>()
        .await?;

    let amount_str = resp["data"]["amount"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing data.amount in Coinbase response"))?;

    let price = Decimal::from_str(amount_str)?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    Ok(BtcPrice {
        price,
        timestamp,
        source: PriceSource::Coinbase,
    })
}

// ── Binance ticker parser ─────────────────────────────────────────────────────

fn parse_binance_ticker(text: &str) -> Option<BtcPrice> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let price_str = v["c"].as_str()?;
    let timestamp = v["E"].as_i64().unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64
    });
    let price = Decimal::from_str(price_str).ok()?;
    Some(BtcPrice {
        price,
        timestamp,
        source: PriceSource::Binance,
    })
}

// ── Polymarket Orderbook Feed ──────────────────────────────────────────────────

/// Connect to Polymarket market WS and subscribe to book + trade channels.
/// Auto-reconnects with 3s backoff.
pub async fn run_polymarket_feed(
    ws_url: String,
    token_ids: Vec<String>,
    state: Arc<crate::types::BotState>,
    tx: broadcast::Sender<DataEvent>,
) {
    loop {
        info!("Connecting to Polymarket WS: {}", ws_url);
        match connect_async(&ws_url).await {
            Ok((ws_stream, _)) => {
                info!("Polymarket WS connected");
                let (mut write, mut read) = ws_stream.split();

                let assets_ids: Vec<serde_json::Value> =
                    token_ids.iter().map(|id| serde_json::json!(id)).collect();

                let sub_msg = serde_json::json!({
                    "type": "subscribe",
                    "channel": "book",
                    "assets_ids": assets_ids
                });
                if let Err(e) = write
                    .send(Message::Text(sub_msg.to_string().into()))
                    .await
                {
                    error!("Failed to send subscribe: {}", e);
                    continue;
                }

                let trade_sub = serde_json::json!({
                    "type": "subscribe",
                    "channel": "trade",
                    "assets_ids": assets_ids
                });
                let _ = write
                    .send(Message::Text(trade_sub.to_string().into()))
                    .await;

                while let Some(msg) = read.next().await {
                    match msg {
                        Ok(Message::Text(text)) => {
                            if let Some((token_id, book)) = parse_orderbook_message(&text) {
                                state.order_books.insert(token_id.clone(), book.clone());
                                let _ = tx.send(DataEvent::OrderBookUpdate { token_id, book });
                            }
                        }
                        Ok(Message::Ping(data)) => {
                            let _ = write.send(Message::Pong(data)).await;
                        }
                        Ok(Message::Close(_)) => {
                            warn!("Polymarket WS closed");
                            break;
                        }
                        Err(e) => {
                            error!("Polymarket WS error: {}", e);
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                error!("Polymarket WS connect error: {}", e);
            }
        }
        warn!("Polymarket WS disconnected, reconnecting in 3s...");
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

fn parse_orderbook_message(text: &str) -> Option<(String, OrderBook)> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;

    let data = if v.is_array() {
        v.get(0)?.clone()
    } else {
        v.clone()
    };

    let event_type = data["event_type"].as_str().unwrap_or("");
    if event_type != "book" && event_type != "price_change" {
        return None;
    }

    let asset_id = data["asset_id"].as_str()?.to_string();

    let parse_levels = |arr: &serde_json::Value| -> Vec<PriceLevel> {
        arr.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|item| {
                        let price = Decimal::from_str(item["price"].as_str()?).ok()?;
                        let size = Decimal::from_str(item["size"].as_str()?).ok()?;
                        Some(PriceLevel { price, size })
                    })
                    .collect()
            })
            .unwrap_or_default()
    };

    let bids = parse_levels(&data["bids"]);
    let asks = parse_levels(&data["asks"]);
    let timestamp = data["timestamp"]
        .as_str()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);

    Some((
        asset_id.clone(),
        OrderBook {
            token_id: asset_id,
            bids,
            asks,
            timestamp,
        },
    ))
}

// ── REST Fallback (Polymarket orderbook) ─────────────────────────────────────

pub async fn fetch_orderbook_rest(
    clob_url: &str,
    token_id: &str,
    client: &Client,
) -> Result<OrderBook> {
    let url = format!("{}/book?token_id={}", clob_url, token_id);
    let resp = client
        .get(&url)
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;

    let parse_levels = |arr: &serde_json::Value| -> Vec<PriceLevel> {
        arr.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|item| {
                        let price = Decimal::from_str(item["price"].as_str()?).ok()?;
                        let size = Decimal::from_str(item["size"].as_str()?).ok()?;
                        Some(PriceLevel { price, size })
                    })
                    .collect()
            })
            .unwrap_or_default()
    };

    Ok(OrderBook {
        token_id: token_id.to_string(),
        bids: parse_levels(&resp["bids"]),
        asks: parse_levels(&resp["asks"]),
        timestamp: 0,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_coinbase_response() {
        let json = r#"{"data":{"base":"BTC","currency":"USD","amount":"65432.10"}}"#;
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        let amount = v["data"]["amount"].as_str().unwrap();
        let price = Decimal::from_str(amount).unwrap();
        assert_eq!(price, Decimal::from_str("65432.10").unwrap());
    }

    #[test]
    fn test_parse_binance_ticker() {
        let json = r#"{"e":"24hrTicker","E":1710000000000,"c":"65000.50","v":"1234.5"}"#;
        let result = parse_binance_ticker(json);
        assert!(result.is_some());
        let p = result.unwrap();
        assert_eq!(p.source, PriceSource::Binance);
        assert_eq!(p.price, Decimal::from_str("65000.50").unwrap());
        assert_eq!(p.timestamp, 1710000000000);
    }

    #[test]
    fn test_parse_binance_ticker_missing_price() {
        let json = r#"{"e":"ping"}"#;
        assert!(parse_binance_ticker(json).is_none());
    }
}
