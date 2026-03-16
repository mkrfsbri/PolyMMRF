use crate::types::{BtcPrice, DataEvent, OrderBook, PriceLevel, PriceSource};
use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

// ── Binance Feed ───────────────────────────────────────────────────────────────

/// Connect to Binance BTC/USDT ticker WebSocket and broadcast price updates.
/// Auto-reconnects on disconnect with 3s backoff.
pub async fn run_binance_feed(
    ws_url: String,
    state: Arc<crate::types::BotState>,
    tx: broadcast::Sender<DataEvent>,
) {
    loop {
        info!("Connecting to Binance WebSocket: {}", ws_url);
        match connect_async(&ws_url).await {
            Ok((ws_stream, _)) => {
                info!("Binance WebSocket connected");
                let (mut write, mut read) = ws_stream.split();
                while let Some(msg) = read.next().await {
                    match msg {
                        Ok(Message::Text(text)) => {
                            if let Some(price) = parse_binance_ticker(&text) {
                                debug!("BTC price: {}", price.price);
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
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                error!("Binance WS connect error: {}", e);
            }
        }
        warn!("Binance WS disconnected, reconnecting in 3s...");
        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
    }
}

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

                // Subscribe to book channel for each token
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

                // Also subscribe to trade channel
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
                            if let Some((token_id, book)) =
                                parse_orderbook_message(&text)
                            {
                                state.order_books.insert(token_id.clone(), book.clone());
                                let _ = tx.send(DataEvent::OrderBookUpdate {
                                    token_id,
                                    book,
                                });
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
        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
    }
}

fn parse_orderbook_message(text: &str) -> Option<(String, OrderBook)> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;

    // Handle wrapped array format
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

// ── REST Fallback ─────────────────────────────────────────────────────────────

pub async fn fetch_orderbook_rest(
    clob_url: &str,
    token_id: &str,
    client: &reqwest::Client,
) -> Result<OrderBook> {
    let url = format!("{}/book?token_id={}", clob_url, token_id);
    let resp = client.get(&url).send().await?.json::<serde_json::Value>().await?;

    let parse_levels = |arr: &serde_json::Value| -> Vec<PriceLevel> {
        arr.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|item| {
                        let price =
                            Decimal::from_str(item["price"].as_str()?).ok()?;
                        let size =
                            Decimal::from_str(item["size"].as_str()?).ok()?;
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
